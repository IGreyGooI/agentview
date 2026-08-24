use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse_macro_input,
    visit_mut::{self, VisitMut},
    Expr, FnArg, GenericParam, ItemFn, PathArguments, ReturnType, Type,
};

pub(crate) fn expand(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    if !attribute.is_empty() {
        return syn::Error::new_spanned(
            &function.sig.ident,
            "#[component] does not accept arguments",
        )
        .into_compile_error()
        .into();
    }

    expand_function(function)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_function(mut function: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if let Some(asyncness) = function.sig.asyncness {
        return Err(syn::Error::new_spanned(
            asyncness,
            "Component render functions must be synchronous",
        ));
    }
    if let Some(constness) = function.sig.constness {
        return Err(syn::Error::new_spanned(
            constness,
            "Component render functions cannot be const",
        ));
    }
    if let Some(unsafety) = function.sig.unsafety {
        return Err(syn::Error::new_spanned(
            unsafety,
            "Component render functions cannot be unsafe",
        ));
    }
    if function.sig.abi.is_some() || function.sig.variadic.is_some() {
        return Err(syn::Error::new_spanned(
            &function.sig,
            "Component render functions must use the ordinary Rust ABI",
        ));
    }
    if function
        .sig
        .generics
        .params
        .iter()
        .any(|parameter| matches!(parameter, GenericParam::Type(_) | GenericParam::Const(_)))
        || !function.sig.generics.params.is_empty()
    {
        return Err(syn::Error::new_spanned(
            &function.sig.generics,
            "Component render functions cannot be generic",
        ));
    }

    for input in &function.sig.inputs {
        let FnArg::Typed(input) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "Component render functions do not support a self receiver",
            ));
        };
        match input.ty.as_ref() {
            Type::Reference(_) => {
                return Err(syn::Error::new_spanned(
                    &input.ty,
                    "Component props must be owned; borrowed props cannot outlive the deferred render",
                ));
            }
            Type::ImplTrait(_) => {
                return Err(syn::Error::new_spanned(
                    &input.ty,
                    "Component props need a concrete owned type",
                ));
            }
            _ => {}
        }
    }

    let ReturnType::Type(_, output) = &function.sig.output else {
        return Err(syn::Error::new_spanned(
            &function.sig.ident,
            "Component render functions must return Component",
        ));
    };
    let Type::Path(output) = output.as_ref() else {
        return Err(syn::Error::new_spanned(
            output,
            "Component render functions must return non-generic Component",
        ));
    };
    let Some(last) = output.path.segments.last() else {
        return Err(syn::Error::new_spanned(
            output,
            "Component render functions must return non-generic Component",
        ));
    };
    if last.ident != "Component" || !matches!(last.arguments, PathArguments::None) {
        return Err(syn::Error::new_spanned(
            output,
            "Component render functions must return non-generic Component",
        ));
    }

    let name = function.sig.ident.clone();
    let mut body = function.block;
    let mut hooks = HookCallRewriter::default();
    hooks.visit_block_mut(&mut body);

    function.block = if hooks.found {
        let mut bindings = Vec::with_capacity(function.sig.inputs.len());
        for (index, input) in function.sig.inputs.iter_mut().enumerate() {
            let FnArg::Typed(input) = input else {
                unreachable!("self receivers were rejected above")
            };
            let original_pattern = input.pat.clone();
            let argument_type = input.ty.clone();
            let captured = syn::Ident::new(
                &format!("__agentview_component_arg_{index}"),
                proc_macro2::Span::mixed_site(),
            );
            *input.pat = syn::parse_quote!(#captured);
            bindings.push(quote! {
                let #original_pattern: #argument_type =
                    ::agentview::component::authoring::__private::clone_repeatable_input(
                        &#captured,
                    );
            });
        }

        Box::new(syn::parse_quote!({
            ::agentview::component::authoring::__private::defer_repeatable_component(
                ::std::concat!(
                    ::std::module_path!(),
                    "::",
                    ::std::stringify!(#name),
                    "@",
                    ::std::file!(),
                    ":",
                    ::std::line!(),
                    ":",
                    ::std::column!(),
                ),
                move |__agentview_hooks| {
                    #(#bindings)*
                    #body
                },
            )
        }))
    } else {
        Box::new(syn::parse_quote!({
            ::agentview::component::authoring::__private::defer_component(
                ::std::concat!(
                    ::std::module_path!(),
                    "::",
                    ::std::stringify!(#name),
                    "@",
                    ::std::file!(),
                    ":",
                    ::std::line!(),
                    ":",
                    ::std::column!(),
                ),
                move || #body,
            )
        }))
    };

    Ok(quote! { #function })
}

#[derive(Default)]
struct HookCallRewriter {
    found: bool,
    next_signal_site: u32,
}

impl VisitMut for HookCallRewriter {
    fn visit_expr_mut(&mut self, expression: &mut Expr) {
        let Expr::Call(call) = expression else {
            visit_mut::visit_expr_mut(self, expression);
            return;
        };
        let Expr::Path(function) = call.func.as_ref() else {
            visit_mut::visit_expr_mut(self, expression);
            return;
        };
        if !is_signal_hook_call(&function.path, call.args.len()) {
            visit_mut::visit_expr_mut(self, expression);
            return;
        }

        let arguments = call.args.clone();
        let site = self.next_signal_site;
        self.next_signal_site = self
            .next_signal_site
            .checked_add(1)
            .expect("Component signal hook site space exhausted");
        *expression = syn::parse_quote! {
            __agentview_hooks.use_signal_at(#site, #arguments)
        };
        self.found = true;
    }

    fn visit_item_fn_mut(&mut self, _function: &mut ItemFn) {
        // A nested function is a distinct declaration and cannot borrow this
        // Component's hook scope.
    }
}

fn is_signal_hook_call(path: &syn::Path, argument_count: usize) -> bool {
    if argument_count != 1 {
        return false;
    }
    let segments = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        [hook] => hook == "use_signal",
        [crate_name, component, prelude, hook]
            if crate_name == "agentview" && component == "component" && prelude == "prelude" =>
        {
            hook == "use_signal"
        }
        _ => false,
    }
}
