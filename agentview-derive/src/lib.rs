use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, LitStr};

#[proc_macro_derive(AgentView, attributes(agent_view, view))]
pub fn derive_agent_view(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_agent_view(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_agent_view(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ident = input.ident;
    let generics = input.generics;
    let container = container_options(&input.attrs)?;
    if container.display {
        if container.kind.is_some() {
            return Err(syn::Error::new_spanned(
                ident,
                "`display` agent views cannot also set `kind`",
            ));
        }
        if container.diff {
            return Err(syn::Error::new_spanned(
                ident,
                "`display` agent views cannot also set `diff`",
            ));
        }
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        return Ok(quote! {
            impl #impl_generics ::agentview::semantic_view::AgentView for #ident #ty_generics #where_clause {
                fn render_root(&self) -> ::agentview::semantic_view::SemanticFragment {
                    ::agentview::semantic_view::SemanticFragment::Text(
                        ::std::string::ToString::to_string(self)
                    )
                }

                fn render_field(
                    &self,
                    field_name: &'static str,
                ) -> ::agentview::semantic_view::SemanticField {
                    ::agentview::semantic_view::SemanticField::Attr {
                        name: field_name.to_owned(),
                        value: ::std::string::ToString::to_string(self),
                    }
                }
            }

            impl #impl_generics ::agentview::semantic_view::AgentViewRoot for #ident #ty_generics #where_clause {}
        });
    }

    let kind = container
        .kind
        .unwrap_or_else(|| to_snake_case(&ident.to_string()));
    if container.diff {
        return Err(syn::Error::new_spanned(
            ident,
            "`diff` belongs on fields: use `#[view(diff)]`",
        ));
    }
    let Data::Struct(data) = input.data else {
        return Err(syn::Error::new_spanned(
            ident,
            "AgentView can only be derived for structs",
        ));
    };
    let Fields::Named(fields) = data.fields else {
        return Err(syn::Error::new_spanned(
            ident,
            "AgentView requires named struct fields",
        ));
    };

    let mut field_renderers = Vec::new();
    for field in fields.named {
        let field_ident = field.ident.expect("named field");
        let field_name = field_ident.to_string();
        let options = field_options(&field.attrs)?;
        if options.skip {
            if !matches!(options.mode, FieldMode::Default)
                || options.name.is_some()
                || options.diff
                || options.replace
                || options.collection_diff_mode.is_some()
            {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "`skip` cannot be combined with rendering, naming, or diff attributes",
                ));
            }
            continue;
        }
        if options.diff
            && matches!(
                options.mode,
                FieldMode::Attr | FieldMode::Text | FieldMode::Comment | FieldMode::Flatten
            )
        {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "`diff` implies node rendering and cannot be combined with this field mode",
            ));
        }
        let rendered_field_name = options.name.as_deref().unwrap_or(&field_name);
        let is_vec = is_vec_type(&field.ty);
        if options.diff && is_vec && options.collection_diff_mode.is_none() {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "Vec diff fields must choose a collection mode, for example `#[view(diff(append))]`",
            ));
        }
        if options.replace && !options.diff {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "`replace` requires `#[view(diff(replace))]`",
            ));
        }
        if options.collection_diff_mode.is_some() && !options.diff {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "collection diff modes require `#[view(diff(...))]`",
            ));
        }
        if options.replace && options.collection_diff_mode.is_some() {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "`replace` cannot be combined with collection diff modes",
            ));
        }
        if options.collection_diff_mode.is_some() && !is_vec {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "collection diff modes are only supported on Vec fields",
            ));
        }
        let current_field = render_field_expr(
            quote! { self },
            &field_ident,
            rendered_field_name,
            options.mode,
        );
        if options.diff {
            let strategy = if options.replace {
                quote! { ::agentview::semantic_view::SemanticDiffStrategy::Replace }
            } else {
                match &options.collection_diff_mode {
                    Some(CollectionDiffMode::Append) => {
                        quote! { ::agentview::semantic_view::SemanticDiffStrategy::Append }
                    }
                    Some(CollectionDiffMode::Set) => {
                        quote! { ::agentview::semantic_view::SemanticDiffStrategy::Set }
                    }
                    Some(CollectionDiffMode::Seq) => {
                        quote! { ::agentview::semantic_view::SemanticDiffStrategy::Sequence }
                    }
                    Some(CollectionDiffMode::Keyed(key_attr)) => {
                        quote! {
                            ::agentview::semantic_view::SemanticDiffStrategy::Keyed(#key_attr)
                        }
                    }
                    None => {
                        quote! { ::agentview::semantic_view::SemanticDiffStrategy::Recursive }
                    }
                }
            };
            field_renderers.push(quote! {
                node.push_diff_field(
                    #rendered_field_name,
                    #strategy,
                    #current_field,
                );
            });
        } else {
            field_renderers.push(quote! {
                node.push_field(#current_field);
            });
        }
    }

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics ::agentview::semantic_view::AgentView for #ident #ty_generics #where_clause {
            fn render_root(&self) -> ::agentview::semantic_view::SemanticFragment {
                let mut node = ::agentview::semantic_view::SemanticNode::new(#kind);
                #(#field_renderers)*
                ::agentview::semantic_view::SemanticFragment::Node(node)
            }

            fn render_field(
                &self,
                field_name: &'static str,
            ) -> ::agentview::semantic_view::SemanticField {
                let mut node = ::agentview::semantic_view::SemanticNode::new(field_name);
                node.push_attr("kind", #kind);
                #(#field_renderers)*
                ::agentview::semantic_view::SemanticField::Fragment(
                    ::agentview::semantic_view::SemanticFragment::Node(node)
                )
            }

        }

        impl #impl_generics ::agentview::semantic_view::AgentViewRoot for #ident #ty_generics #where_clause {}
    })
}

#[derive(Default)]
struct ContainerOptions {
    kind: Option<String>,
    display: bool,
    diff: bool,
}

fn container_options(attrs: &[syn::Attribute]) -> syn::Result<ContainerOptions> {
    let mut kind = None;
    let mut display = false;
    let mut diff = false;
    for attr in attrs {
        if !attr.path().is_ident("agent_view") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("kind") {
                let value = meta.value()?;
                kind = Some(value.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("display") {
                display = true;
                Ok(())
            } else if meta.path.is_ident("diff") {
                diff = true;
                Ok(())
            } else if meta.path.is_ident("tag") {
                Err(meta.error("use `kind = ...` instead of `tag = ...`"))
            } else {
                Err(meta.error("unsupported agent_view attribute"))
            }
        })?;
    }
    Ok(ContainerOptions {
        kind,
        display,
        diff,
    })
}

#[derive(Clone, Copy)]
enum FieldMode {
    Default,
    Attr,
    Element,
    Text,
    Comment,
    Flatten,
}

fn set_field_mode(
    mode: &mut Option<FieldMode>,
    next: FieldMode,
    meta: &syn::meta::ParseNestedMeta<'_>,
) -> syn::Result<()> {
    if mode.is_some() {
        return Err(meta.error("a field can have only one rendering mode"));
    }
    *mode = Some(next);
    Ok(())
}

#[derive(Clone)]
struct FieldOptions {
    mode: FieldMode,
    name: Option<String>,
    skip: bool,
    diff: bool,
    replace: bool,
    collection_diff_mode: Option<CollectionDiffMode>,
}

#[derive(Clone)]
enum CollectionDiffMode {
    Append,
    Set,
    Seq,
    Keyed(String),
}

fn field_options(attrs: &[syn::Attribute]) -> syn::Result<FieldOptions> {
    let mut mode = None;
    let mut name = None;
    let mut skip = false;
    let mut diff = false;
    let mut replace = false;
    let mut collection_diff_mode = None;
    for attr in attrs {
        if !attr.path().is_ident("view") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("attr") {
                if meta.input.peek(syn::Token![=]) {
                    let value = meta.value()?;
                    name = Some(value.parse::<LitStr>()?.value());
                }
                set_field_mode(&mut mode, FieldMode::Attr, &meta)
            } else if meta.path.is_ident("name") {
                let value = meta.value()?;
                name = Some(value.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("element") {
                set_field_mode(&mut mode, FieldMode::Element, &meta)
            } else if meta.path.is_ident("text") {
                set_field_mode(&mut mode, FieldMode::Text, &meta)
            } else if meta.path.is_ident("comment") {
                set_field_mode(&mut mode, FieldMode::Comment, &meta)
            } else if meta.path.is_ident("children") || meta.path.is_ident("flatten") {
                set_field_mode(&mut mode, FieldMode::Flatten, &meta)
            } else if meta.path.is_ident("skip") {
                skip = true;
                Ok(())
            } else if meta.path.is_ident("diff") {
                diff = true;
                if meta.input.peek(syn::token::Paren) {
                    meta.parse_nested_meta(|nested| {
                        if nested.path.is_ident("append") {
                            collection_diff_mode = Some(CollectionDiffMode::Append);
                            Ok(())
                        } else if nested.path.is_ident("set") {
                            collection_diff_mode = Some(CollectionDiffMode::Set);
                            Ok(())
                        } else if nested.path.is_ident("seq") {
                            collection_diff_mode = Some(CollectionDiffMode::Seq);
                            Ok(())
                        } else if nested.path.is_ident("key")
                            || nested.path.is_ident("key_attr")
                            || nested.path.is_ident("keyed")
                        {
                            let value = nested.value()?;
                            collection_diff_mode =
                                Some(CollectionDiffMode::Keyed(value.parse::<LitStr>()?.value()));
                            Ok(())
                        } else if nested.path.is_ident("replace") {
                            replace = true;
                            Ok(())
                        } else {
                            Err(nested.error("unsupported diff attribute"))
                        }
                    })?;
                }
                Ok(())
            } else if meta.path.is_ident("replace") {
                replace = true;
                Ok(())
            } else if meta.path.is_ident("append") {
                collection_diff_mode = Some(CollectionDiffMode::Append);
                Ok(())
            } else if meta.path.is_ident("set") {
                collection_diff_mode = Some(CollectionDiffMode::Set);
                Ok(())
            } else if meta.path.is_ident("seq") {
                collection_diff_mode = Some(CollectionDiffMode::Seq);
                Ok(())
            } else if meta.path.is_ident("keyed") {
                let value = meta.value()?;
                collection_diff_mode =
                    Some(CollectionDiffMode::Keyed(value.parse::<LitStr>()?.value()));
                Ok(())
            } else {
                Err(meta.error("unsupported view attribute"))
            }
        })?;
    }
    Ok(FieldOptions {
        mode: mode.unwrap_or(FieldMode::Default),
        name,
        skip,
        diff,
        replace,
        collection_diff_mode,
    })
}

fn render_field_expr(
    receiver: proc_macro2::TokenStream,
    field_ident: &syn::Ident,
    field_name: &str,
    mode: FieldMode,
) -> proc_macro2::TokenStream {
    match mode {
        FieldMode::Default => {
            quote! {
                ::agentview::semantic_view::AgentView::render_field(
                    &#receiver.#field_ident,
                    #field_name,
                )
            }
        }
        FieldMode::Attr => {
            quote! {
                ::agentview::semantic_view::SemanticField::Attr {
                    name: #field_name.to_owned(),
                    value: ::agentview::semantic_view::view_value(&#receiver.#field_ident),
                }
            }
        }
        FieldMode::Element => {
            quote! {
                ::agentview::semantic_view::SemanticField::Fragment(
                    ::agentview::semantic_view::SemanticFragment::Node(
                        ::agentview::semantic_view::SemanticNode::element(
                            #field_name,
                            ::agentview::semantic_view::view_value(&#receiver.#field_ident),
                        )
                    )
                )
            }
        }
        FieldMode::Text => {
            quote! {
                ::agentview::semantic_view::SemanticField::Fragment(
                    ::agentview::semantic_view::SemanticFragment::Text(
                        ::agentview::semantic_view::view_value(&#receiver.#field_ident),
                    )
                )
            }
        }
        FieldMode::Comment => {
            quote! {
                ::agentview::semantic_view::SemanticField::Fragment(
                    ::agentview::semantic_view::SemanticFragment::Comment(
                        ::agentview::semantic_view::view_value(&#receiver.#field_ident),
                    )
                )
            }
        }
        FieldMode::Flatten => {
            quote! {
                ::agentview::semantic_view::render_children_field(&#receiver.#field_ident)
            }
        }
    }
}

fn is_vec_type(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Vec")
}

fn to_snake_case(input: &str) -> String {
    let mut output = String::new();
    for (index, ch) in input.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                output.push('_');
            }
            for lower in ch.to_lowercase() {
                output.push(lower);
            }
        } else {
            output.push(ch);
        }
    }
    output
}
