use proc_macro::TokenStream;

use quote::quote;
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    Attribute, Error, Expr, ExprLit, FnArg, GenericArgument, Ident, ItemFn, Lit, LitStr, Meta, Pat,
    PathArguments, ReturnType, Token, Type, Visibility,
};

pub(crate) fn expand(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let options = match syn::parse::<ToolOptions>(attribute) {
        Ok(options) => options,
        Err(error) => return error.into_compile_error().into(),
    };
    let function = parse_macro_input!(item as ItemFn);

    expand_function(options, function)
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[derive(Default)]
struct ToolOptions {
    name: Option<LitStr>,
    description: Option<LitStr>,
}

impl Parse for ToolOptions {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let attributes = Punctuated::<Meta, Token![,]>::parse_terminated(input)?;
        let mut options = Self::default();
        for attribute in attributes {
            let Meta::NameValue(attribute) = attribute else {
                return Err(Error::new_spanned(
                    attribute,
                    "expected `name = \"...\"` or `description = \"...\"`",
                ));
            };
            let value = string_literal(&attribute.value)?;
            if attribute.path.is_ident("name") {
                if options.name.replace(value).is_some() {
                    return Err(Error::new_spanned(
                        attribute.path,
                        "duplicate `name` option",
                    ));
                }
            } else if attribute.path.is_ident("description") {
                if options.description.replace(value).is_some() {
                    return Err(Error::new_spanned(
                        attribute.path,
                        "duplicate `description` option",
                    ));
                }
            } else {
                return Err(Error::new_spanned(
                    attribute.path,
                    "unsupported #[tool] option; expected `name` or `description`",
                ));
            }
        }
        Ok(options)
    }
}

fn string_literal(expression: &Expr) -> syn::Result<LitStr> {
    let Expr::Lit(ExprLit {
        lit: Lit::Str(value),
        ..
    }) = expression
    else {
        return Err(Error::new_spanned(expression, "expected a string literal"));
    };
    Ok(value.clone())
}

struct ToolArgument {
    ident: Ident,
    ty: Type,
    attrs: Vec<Attribute>,
}

fn expand_function(
    options: ToolOptions,
    mut function: ItemFn,
) -> syn::Result<proc_macro2::TokenStream> {
    validate_signature(&function)?;
    let arguments = collect_arguments(&function)?;
    let (output, error) = result_types(&function.sig.output)?;

    let function_ident = function.sig.ident.clone();
    let function_name = unraw_ident(&function_ident);
    let tool_name = options
        .name
        .unwrap_or_else(|| LitStr::new(&function_name, function_ident.span()));
    if tool_name.value().is_empty() {
        return Err(Error::new_spanned(tool_name, "tool names cannot be empty"));
    }
    let description = options
        .description
        .unwrap_or_else(|| LitStr::new(&doc_description(&function.attrs), function_ident.span()));

    let tool_ident = generated_type_ident(&function_ident, "Tool");
    let args_ident = generated_type_ident(&function_ident, "Args");
    let handler_ident = Ident::new("__agentview_call", proc_macro2::Span::mixed_site());
    let visibility = function.vis.clone();
    let cfg_attributes = function
        .attrs
        .iter()
        .filter(|attribute| is_cfg_attribute(attribute))
        .cloned()
        .collect::<Vec<_>>();
    let doc_attributes = function
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
        .cloned()
        .collect::<Vec<_>>();

    function.vis = Visibility::Inherited;
    function.sig.ident = handler_ident.clone();
    for input in &mut function.sig.inputs {
        let FnArg::Typed(argument) = input else {
            unreachable!("self receivers were rejected before handler generation")
        };
        // Parameter attributes belong on the generated argument-object field.
        // Keeping them on the private Rust function would reject `doc`,
        // `serde`, and `schemars` attributes before the handler can compile.
        argument.attrs.clear();
    }

    let fields = arguments.iter().map(|argument| {
        let ToolArgument { ident, ty, attrs } = argument;
        quote! {
            #(#attrs)*
            pub #ident: #ty
        }
    });
    let values = arguments.iter().map(|argument| {
        let ident = &argument.ident;
        quote! { args.#ident }
    });
    let invoke = if function.sig.asyncness.is_some() {
        quote! { Self::#handler_ident(#(#values),*).await }
    } else {
        quote! { Self::#handler_ident(#(#values),*) }
    };

    Ok(quote! {
        #(#cfg_attributes)*
        #[doc(hidden)]
        #[derive(
            ::agentview::__private::serde::Deserialize,
            ::agentview::__private::schemars::JsonSchema,
        )]
        #[serde(
            crate = "::agentview::__private::serde",
            deny_unknown_fields,
        )]
        #[schemars(crate = "::agentview::__private::schemars")]
        #visibility struct #args_ident {
            #(#fields,)*
        }

        #(#cfg_attributes)*
        #[doc(hidden)]
        #[derive(Clone, Copy, Debug, Default)]
        #visibility struct #tool_ident;

        #(#cfg_attributes)*
        impl #tool_ident {
            #function
        }

        #(#cfg_attributes)*
        impl ::agentview::component::authoring::NativeTool for #tool_ident {
            type Args = #args_ident;
            type Output = #output;
            type Error = #error;

            const NAME: &'static str = #tool_name;
            const DESCRIPTION: &'static str = #description;

            fn call(
                &self,
                args: Self::Args,
            ) -> impl ::std::future::Future<
                Output = ::std::result::Result<Self::Output, Self::Error>,
            > + ::std::marker::Send {
                async move { #invoke }
            }
        }

        #(#cfg_attributes)*
        #(#doc_attributes)*
        #[allow(non_upper_case_globals)]
        #visibility const #function_ident: #tool_ident = #tool_ident;
    })
}

fn validate_signature(function: &ItemFn) -> syn::Result<()> {
    if function.sig.constness.is_some() {
        return Err(Error::new_spanned(
            &function.sig,
            "#[tool] functions cannot be const",
        ));
    }
    if function.sig.unsafety.is_some() {
        return Err(Error::new_spanned(
            &function.sig,
            "#[tool] functions cannot be unsafe",
        ));
    }
    if function.sig.abi.is_some() || function.sig.variadic.is_some() {
        return Err(Error::new_spanned(
            &function.sig,
            "#[tool] functions must use the ordinary Rust ABI",
        ));
    }
    if !function.sig.generics.params.is_empty() || function.sig.generics.where_clause.is_some() {
        return Err(Error::new_spanned(
            &function.sig.generics,
            "#[tool] functions cannot be generic",
        ));
    }
    for input in &function.sig.inputs {
        let FnArg::Typed(argument) = input else {
            return Err(Error::new_spanned(
                input,
                "#[tool] functions cannot take a self receiver",
            ));
        };
        let Pat::Ident(pattern) = argument.pat.as_ref() else {
            return Err(Error::new_spanned(
                &argument.pat,
                "#[tool] parameters must use simple identifier patterns",
            ));
        };
        if pattern.by_ref.is_some() || pattern.subpat.is_some() {
            return Err(Error::new_spanned(
                pattern,
                "#[tool] parameters must use simple owned identifier patterns",
            ));
        }
        validate_argument_attributes(&argument.attrs)?;
        validate_owned_type(&argument.ty)?;
    }
    result_types(&function.sig.output).map(|_| ())
}

fn collect_arguments(function: &ItemFn) -> syn::Result<Vec<ToolArgument>> {
    function
        .sig
        .inputs
        .iter()
        .map(|input| {
            let FnArg::Typed(argument) = input else {
                unreachable!("self receivers were rejected before collection")
            };
            let Pat::Ident(pattern) = argument.pat.as_ref() else {
                unreachable!("non-identifier patterns were rejected before collection")
            };
            Ok(ToolArgument {
                ident: pattern.ident.clone(),
                ty: argument.ty.as_ref().clone(),
                attrs: argument.attrs.clone(),
            })
        })
        .collect()
}

fn validate_argument_attributes(attributes: &[Attribute]) -> syn::Result<()> {
    for attribute in attributes {
        if !attribute.path().is_ident("serde") {
            continue;
        }
        let Meta::List(list) = &attribute.meta else {
            continue;
        };
        let options = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        if let Some(flatten) = options
            .iter()
            .find(|option| option.path().is_ident("flatten"))
        {
            return Err(Error::new_spanned(
                flatten,
                "#[tool] parameters do not support #[serde(flatten)]",
            ));
        }
    }
    Ok(())
}

fn result_types(output: &ReturnType) -> syn::Result<(Type, Type)> {
    let ReturnType::Type(_, output) = output else {
        return Err(Error::new_spanned(
            output,
            "#[tool] functions must explicitly return `Result<Output, Error>`",
        ));
    };
    let Type::Path(path) = output.as_ref() else {
        return Err(Error::new_spanned(
            output,
            "#[tool] functions must return `Result<Output, Error>`",
        ));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(Error::new_spanned(
            output,
            "#[tool] functions must return `Result<Output, Error>`",
        ));
    };
    if segment.ident != "Result" {
        return Err(Error::new_spanned(
            output,
            "#[tool] functions must return `Result<Output, Error>`",
        ));
    }
    let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(Error::new_spanned(
            output,
            "#[tool] functions must return `Result<Output, Error>`",
        ));
    };
    let types = arguments
        .args
        .iter()
        .filter_map(|argument| match argument {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if arguments.args.len() != 2 || types.len() != 2 {
        return Err(Error::new_spanned(
            output,
            "#[tool] functions must return `Result<Output, Error>`",
        ));
    }
    Ok((types[0].clone(), types[1].clone()))
}

fn validate_owned_type(ty: &Type) -> syn::Result<()> {
    match ty {
        Type::Reference(_) => Err(Error::new_spanned(
            ty,
            "#[tool] parameters must be owned; borrowed parameters are unsupported",
        )),
        Type::ImplTrait(_) | Type::TraitObject(_) | Type::BareFn(_) | Type::Infer(_) => Err(
            Error::new_spanned(ty, "#[tool] parameters need concrete owned types"),
        ),
        Type::Ptr(_) | Type::Slice(_) => Err(Error::new_spanned(
            ty,
            "#[tool] parameters need JSON-decodable owned types",
        )),
        Type::Array(array) => validate_owned_type(&array.elem),
        Type::Group(group) => validate_owned_type(&group.elem),
        Type::Paren(paren) => validate_owned_type(&paren.elem),
        Type::Tuple(tuple) => {
            for element in &tuple.elems {
                validate_owned_type(element)?;
            }
            Ok(())
        }
        Type::Path(path) => {
            for segment in &path.path.segments {
                validate_path_arguments(&segment.arguments)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_path_arguments(arguments: &PathArguments) -> syn::Result<()> {
    let PathArguments::AngleBracketed(arguments) = arguments else {
        return Ok(());
    };
    for argument in &arguments.args {
        match argument {
            GenericArgument::Type(ty) => validate_owned_type(ty)?,
            GenericArgument::AssocType(association) => validate_owned_type(&association.ty)?,
            GenericArgument::Lifetime(lifetime) => {
                return Err(Error::new_spanned(
                    lifetime,
                    "#[tool] parameters cannot contain borrowed lifetimes",
                ));
            }
            GenericArgument::Constraint(constraint) => {
                return Err(Error::new_spanned(
                    constraint,
                    "#[tool] parameters need concrete owned types",
                ));
            }
            GenericArgument::Const(_) | GenericArgument::AssocConst(_) => {}
            _ => {}
        }
    }
    Ok(())
}

fn is_cfg_attribute(attribute: &Attribute) -> bool {
    attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr")
}

fn doc_description(attributes: &[Attribute]) -> String {
    let description = attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
        .filter_map(|attribute| match &attribute.meta {
            Meta::NameValue(value) => match &value.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(value),
                    ..
                }) => Some(value.value().trim().to_owned()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    description.trim().to_owned()
}

fn generated_type_ident(function: &Ident, suffix: &str) -> Ident {
    let mut name = String::new();
    for word in unraw_ident(function)
        .split('_')
        .filter(|word| !word.is_empty())
    {
        let mut characters = word.chars();
        if let Some(first) = characters.next() {
            name.extend(first.to_uppercase());
            name.push_str(characters.as_str());
        }
    }
    name.push_str(suffix);
    Ident::new(&name, function.span())
}

fn unraw_ident(ident: &Ident) -> String {
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}
