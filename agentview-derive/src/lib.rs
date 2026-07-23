use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, LitInt, LitStr};

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
            impl #impl_generics ::agentview::agent_view::AgentView for #ident #ty_generics #where_clause {
                type Root = ::agentview::pom::TextNode;

                fn build_root(
                    &self,
                ) -> ::std::result::Result<Self::Root, ::agentview::pom::PomError> {
                    ::std::result::Result::Ok(::agentview::pom::TextNode::new(
                        ::std::string::ToString::to_string(self),
                    ))
                }
            }

            impl #impl_generics ::agentview::agent_view::AgentViewValue for #ident #ty_generics #where_clause {
                fn build_field(
                    &self,
                    role: ::agentview::pom::XmlName,
                ) -> ::std::result::Result<
                    ::agentview::agent_view::ViewField,
                    ::agentview::pom::PomError,
                > {
                    ::std::result::Result::Ok(
                        ::agentview::agent_view::ViewField::Attribute(
                            ::agentview::pom::XmlAttribute::new(
                                role,
                                ::std::string::ToString::to_string(self),
                            ),
                        ),
                    )
                }

                fn build_children(
                    &self,
                ) -> ::std::result::Result<
                    ::agentview::pom::MixedChildren,
                    ::agentview::pom::PomError,
                > {
                    let mut children = ::agentview::pom::MixedChildren::new();
                    children.push(::agentview::pom::MixedContent::text(
                        ::agentview::pom::TextNode::new(
                            ::std::string::ToString::to_string(self),
                        ),
                    ));
                    ::std::result::Result::Ok(children)
                }
            }

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

    if container.diff {
        return Err(syn::Error::new_spanned(
            &ident,
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
            &ident,
            "AgentView requires named struct fields",
        ));
    };

    if container.document {
        return expand_document_view(ident, generics, fields);
    }
    if let Some(MarkdownContainer::Paragraph) = container.markdown {
        return expand_paragraph_view(ident, generics, fields);
    }

    let kind = container
        .kind
        .unwrap_or_else(|| to_snake_case(&ident.to_string()));
    validate_inferred_xml_name(
        &kind,
        ident.span(),
        "type name does not infer a valid XML kind; set `#[agent_view(kind = \"...\")]`",
    )?;

    let mut legacy_field_renderers = Vec::new();
    let mut pom_field_renderers = Vec::new();
    let mut legacy_compatible = true;
    for field in fields.named {
        let field_ident = field.ident.expect("named field");
        let field_name = field_ident.to_string();
        let field_name = field_name
            .strip_prefix("r#")
            .unwrap_or(&field_name)
            .to_owned();
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
        if options.name.is_some()
            && (matches!(options.mode, FieldMode::Text | FieldMode::Flatten)
                || matches!(options.mode, FieldMode::Root) && !options.diff)
        {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "`name` is not valid for this field mode",
            ));
        }
        match options.mode {
            FieldMode::Default
            | FieldMode::Attr
            | FieldMode::Element
            | FieldMode::Text
            | FieldMode::Flatten
            | FieldMode::Root
            | FieldMode::CodeSpan => {}
            FieldMode::Comment => {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "`comment` has no Prompt Object Model representation; use an explicit XML element",
                ));
            }
            FieldMode::Xml
            | FieldMode::Paragraph
            | FieldMode::Heading(_)
            | FieldMode::OrderedList
            | FieldMode::UnorderedList => {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "this field mode is only valid in a document view",
                ));
            }
        }
        if matches!(options.mode, FieldMode::Root | FieldMode::CodeSpan) {
            legacy_compatible = false;
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
        let inferred_name_is_used = options.name.is_none()
            && (matches!(
                options.mode,
                FieldMode::Default | FieldMode::Attr | FieldMode::Element | FieldMode::CodeSpan
            ) || options.diff);
        if inferred_name_is_used {
            validate_inferred_xml_name(
                &field_name,
                field_ident.span(),
                "field name does not infer a valid XML name; set `#[view(name = \"...\")]`",
            )?;
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
        let pom_current_field = pom_field_expr(
            quote! { self },
            &field_ident,
            rendered_field_name,
            options.mode,
        );
        let legacy_current_field = if legacy_compatible {
            Some(render_field_expr(
                quote! { self },
                &field_ident,
                rendered_field_name,
                options.mode,
            ))
        } else {
            None
        };
        if options.diff {
            let legacy_strategy =
                legacy_diff_strategy(options.replace, options.collection_diff_mode.as_ref());
            let pom_strategy =
                pom_diff_strategy(options.replace, options.collection_diff_mode.as_ref());
            pom_field_renderers.push(quote! {
                ::agentview::agent_view::push_diff_view_field(
                    &mut node,
                    ::agentview::pom::XmlName::try_from(#rendered_field_name)?,
                    #pom_strategy,
                    #pom_current_field,
                )?;
            });
            if let Some(current_field) = legacy_current_field {
                legacy_field_renderers.push(quote! {
                    node.push_diff_field(
                        #rendered_field_name,
                        #legacy_strategy,
                        #current_field,
                    );
                });
            }
        } else {
            pom_field_renderers.push(quote! {
                ::agentview::agent_view::push_view_field(
                    &mut node,
                    #pom_current_field,
                )?;
            });
            if let Some(current_field) = legacy_current_field {
                legacy_field_renderers.push(quote! {
                    node.push_field(#current_field);
                });
            }
        }
    }

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let legacy_impl = legacy_compatible.then(|| {
        quote! {
        impl #impl_generics ::agentview::semantic_view::AgentView for #ident #ty_generics #where_clause {
            fn render_root(&self) -> ::agentview::semantic_view::SemanticFragment {
                let mut node = ::agentview::semantic_view::SemanticNode::new(#kind);
                #(#legacy_field_renderers)*
                ::agentview::semantic_view::SemanticFragment::Node(node)
            }

            fn render_field(
                &self,
                field_name: &'static str,
            ) -> ::agentview::semantic_view::SemanticField {
                let mut node = ::agentview::semantic_view::SemanticNode::new(field_name);
                node.push_attr("kind", #kind);
                #(#legacy_field_renderers)*
                ::agentview::semantic_view::SemanticField::Fragment(
                    ::agentview::semantic_view::SemanticFragment::Node(node)
                )
            }

        }

        impl #impl_generics ::agentview::semantic_view::AgentViewRoot for #ident #ty_generics #where_clause {}
        }
    });

    Ok(quote! {
        impl #impl_generics ::agentview::agent_view::AgentView for #ident #ty_generics #where_clause {
            type Root = ::agentview::pom::XmlNode;

            fn build_root(
                &self,
            ) -> ::std::result::Result<Self::Root, ::agentview::pom::PomError> {
                let mut node = ::agentview::pom::XmlNode::new(
                    ::agentview::pom::XmlName::try_from(#kind)?,
                );
                #(#pom_field_renderers)*
                ::std::result::Result::Ok(node)
            }
        }

        impl #impl_generics ::agentview::agent_view::AgentViewValue for #ident #ty_generics #where_clause {
            fn build_field(
                &self,
                role: ::agentview::pom::XmlName,
            ) -> ::std::result::Result<
                ::agentview::agent_view::ViewField,
                ::agentview::pom::PomError,
            > {
                let mut node = ::agentview::pom::XmlNode::new(role);
                node.push_attribute(
                    ::agentview::pom::XmlName::try_from("kind")?,
                    #kind,
                )?;
                #(#pom_field_renderers)*
                ::std::result::Result::Ok(
                    ::agentview::agent_view::ViewField::Content(
                        ::agentview::pom::MixedContent::xml(node),
                    ),
                )
            }

            fn build_children(
                &self,
            ) -> ::std::result::Result<
                ::agentview::pom::MixedChildren,
                ::agentview::pom::PomError,
            > {
                let node =
                    <Self as ::agentview::agent_view::AgentView>::build_root(self)?;
                let mut children = ::agentview::pom::MixedChildren::new();
                children.push(::agentview::pom::MixedContent::xml(node));
                ::std::result::Result::Ok(children)
            }
        }

        #legacy_impl
    })
}

fn expand_document_view(
    ident: syn::Ident,
    generics: syn::Generics,
    fields: syn::FieldsNamed,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut block_builders = Vec::new();
    for field in fields.named {
        let field_ident = field.ident.expect("named field");
        let options = field_options(&field.attrs)?;
        if options.skip {
            validate_skipped_field(&field_ident, &options)?;
            continue;
        }
        validate_non_diff_field(&field_ident, &options, "document")?;
        if options.name.is_some() {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "`name` is not valid on document fields",
            ));
        }

        let block = match options.mode {
            FieldMode::Heading(level) => quote! {
                document.try_heading(#level, |inline| {
                    inline.try_text(::agentview::agent_view::view_value(
                        &self.#field_ident,
                    ))
                })?;
            },
            FieldMode::Paragraph => quote! {
                document.try_paragraph(|inline| {
                    inline.try_text(::agentview::agent_view::view_value(
                        &self.#field_ident,
                    ))
                })?;
            },
            FieldMode::OrderedList | FieldMode::UnorderedList => {
                let kind = if matches!(options.mode, FieldMode::OrderedList) {
                    quote! { ::agentview::pom::ListKind::Ordered { start: 1 } }
                } else {
                    quote! { ::agentview::pom::ListKind::Unordered }
                };
                quote! {
                    document.try_list(#kind, |list| {
                        for item in &self.#field_ident {
                            let paragraph =
                                ::agentview::agent_view::AgentView::build_root(item)?;
                            list.try_item(|blocks| {
                                blocks.push(
                                    ::agentview::pom::BlockContent::paragraph(paragraph),
                                );
                                ::std::result::Result::Ok(())
                            })?;
                        }
                        ::std::result::Result::Ok(())
                    })?;
                }
            }
            FieldMode::Xml => quote! {
                document.xml(
                    ::agentview::agent_view::AgentView::build_root(
                        &self.#field_ident,
                    )?,
                );
            },
            FieldMode::Default => {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "document fields need an explicit block mode such as `heading`, `paragraph`, `ordered_list`, or `xml`",
                ));
            }
            FieldMode::Attr
            | FieldMode::Element
            | FieldMode::Text
            | FieldMode::Comment
            | FieldMode::Flatten
            | FieldMode::Root
            | FieldMode::CodeSpan => {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "this field mode is not valid in a document view",
                ));
            }
        };
        block_builders.push(block);
    }

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::agentview::agent_view::AgentView for #ident #ty_generics #where_clause {
            type Root = ::agentview::pom::Document;

            fn build_root(
                &self,
            ) -> ::std::result::Result<Self::Root, ::agentview::pom::PomError> {
                ::agentview::pom::Document::try_build(|document| {
                    #(#block_builders)*
                    ::std::result::Result::Ok(())
                })
            }
        }
    })
}

fn expand_paragraph_view(
    ident: syn::Ident,
    generics: syn::Generics,
    fields: syn::FieldsNamed,
) -> syn::Result<proc_macro2::TokenStream> {
    let mut inline_builders = Vec::new();
    for field in fields.named {
        let field_ident = field.ident.expect("named field");
        let options = field_options(&field.attrs)?;
        if options.skip {
            validate_skipped_field(&field_ident, &options)?;
            continue;
        }
        validate_non_diff_field(&field_ident, &options, "Markdown paragraph")?;
        if options.name.is_some() {
            return Err(syn::Error::new_spanned(
                &field_ident,
                "`name` is not valid on Markdown paragraph fields",
            ));
        }

        let inline = match options.mode {
            FieldMode::Text => quote! {
                children.push(::agentview::pom::InlineContent::try_text(
                    ::agentview::agent_view::view_value(&self.#field_ident),
                )?);
            },
            FieldMode::CodeSpan => quote! {
                children.push(::agentview::pom::InlineContent::code_span(
                    ::agentview::pom::CodeSpanNode::new(
                        ::agentview::pom::TextNode::new(
                            ::agentview::agent_view::view_value(&self.#field_ident),
                        ),
                    ),
                ));
            },
            FieldMode::Default => {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "Markdown paragraph fields need an explicit `text` or `code_span` mode",
                ));
            }
            FieldMode::Attr
            | FieldMode::Element
            | FieldMode::Comment
            | FieldMode::Flatten
            | FieldMode::Root
            | FieldMode::Xml
            | FieldMode::Paragraph
            | FieldMode::Heading(_)
            | FieldMode::OrderedList
            | FieldMode::UnorderedList => {
                return Err(syn::Error::new_spanned(
                    &field_ident,
                    "this field mode is not valid in a Markdown paragraph view",
                ));
            }
        };
        inline_builders.push(inline);
    }

    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::agentview::agent_view::AgentView for #ident #ty_generics #where_clause {
            type Root = ::agentview::pom::ParagraphNode;

            fn build_root(
                &self,
            ) -> ::std::result::Result<Self::Root, ::agentview::pom::PomError> {
                let mut children = ::agentview::pom::InlineChildren::new();
                #(#inline_builders)*
                ::std::result::Result::Ok(
                    ::agentview::pom::ParagraphNode::new(children),
                )
            }
        }
    })
}

fn validate_skipped_field(field_ident: &syn::Ident, options: &FieldOptions) -> syn::Result<()> {
    if !matches!(options.mode, FieldMode::Default)
        || options.name.is_some()
        || options.diff
        || options.replace
        || options.collection_diff_mode.is_some()
    {
        return Err(syn::Error::new_spanned(
            field_ident,
            "`skip` cannot be combined with rendering, naming, or diff attributes",
        ));
    }
    Ok(())
}

fn validate_non_diff_field(
    field_ident: &syn::Ident,
    options: &FieldOptions,
    container: &str,
) -> syn::Result<()> {
    if options.diff || options.replace || options.collection_diff_mode.is_some() {
        return Err(syn::Error::new_spanned(
            field_ident,
            format!("diff modes are not valid in a {container} view"),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct ContainerOptions {
    kind: Option<String>,
    display: bool,
    document: bool,
    markdown: Option<MarkdownContainer>,
    diff: bool,
}

#[derive(Clone, Copy)]
enum MarkdownContainer {
    Paragraph,
}

fn container_options(attrs: &[syn::Attribute]) -> syn::Result<ContainerOptions> {
    let mut kind = None;
    let mut display = false;
    let mut document = false;
    let mut markdown = None;
    let mut diff = false;
    for attr in attrs {
        if !attr.path().is_ident("agent_view") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("kind") {
                let value = meta.value()?.parse::<LitStr>()?;
                validate_xml_name_literal(&value, "invalid XML name in `kind`")?;
                kind = Some(value.value());
                Ok(())
            } else if meta.path.is_ident("display") {
                display = true;
                Ok(())
            } else if meta.path.is_ident("document") {
                document = true;
                Ok(())
            } else if meta.path.is_ident("markdown") {
                let value = meta.value()?.parse::<LitStr>()?;
                markdown = Some(match value.value().as_str() {
                    "paragraph" => MarkdownContainer::Paragraph,
                    _ => {
                        return Err(syn::Error::new_spanned(
                            value,
                            "unsupported markdown root; expected `paragraph`",
                        ))
                    }
                });
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
    let root_mode_count = usize::from(kind.is_some())
        + usize::from(display)
        + usize::from(document)
        + usize::from(markdown.is_some());
    if root_mode_count > 1 {
        return Err(syn::Error::new(
            proc_macro2::Span::call_site(),
            "`kind`, `display`, `document`, and `markdown` are mutually exclusive",
        ));
    }

    Ok(ContainerOptions {
        kind,
        display,
        document,
        markdown,
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
    Root,
    Xml,
    CodeSpan,
    Paragraph,
    Heading(u8),
    OrderedList,
    UnorderedList,
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

fn set_collection_diff_mode(
    mode: &mut Option<CollectionDiffMode>,
    next: CollectionDiffMode,
    meta: &syn::meta::ParseNestedMeta<'_>,
) -> syn::Result<()> {
    if mode.is_some() {
        return Err(meta.error("a collection diff field can have only one strategy"));
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
                    let value = value.parse::<LitStr>()?;
                    validate_xml_name_literal(&value, "invalid XML field name")?;
                    name = Some(value.value());
                }
                set_field_mode(&mut mode, FieldMode::Attr, &meta)
            } else if meta.path.is_ident("name") {
                let value = meta.value()?;
                let value = value.parse::<LitStr>()?;
                validate_xml_name_literal(&value, "invalid XML field name")?;
                name = Some(value.value());
                Ok(())
            } else if meta.path.is_ident("element") {
                set_field_mode(&mut mode, FieldMode::Element, &meta)
            } else if meta.path.is_ident("text") {
                set_field_mode(&mut mode, FieldMode::Text, &meta)
            } else if meta.path.is_ident("comment") {
                set_field_mode(&mut mode, FieldMode::Comment, &meta)
            } else if meta.path.is_ident("children") || meta.path.is_ident("flatten") {
                set_field_mode(&mut mode, FieldMode::Flatten, &meta)
            } else if meta.path.is_ident("root") {
                set_field_mode(&mut mode, FieldMode::Root, &meta)
            } else if meta.path.is_ident("xml") {
                set_field_mode(&mut mode, FieldMode::Xml, &meta)
            } else if meta.path.is_ident("code_span") {
                set_field_mode(&mut mode, FieldMode::CodeSpan, &meta)
            } else if meta.path.is_ident("paragraph") {
                set_field_mode(&mut mode, FieldMode::Paragraph, &meta)
            } else if meta.path.is_ident("heading") {
                let value = meta.value()?.parse::<LitInt>()?;
                let level = value.base10_parse::<u8>()?;
                if !(1..=6).contains(&level) {
                    return Err(syn::Error::new_spanned(
                        value,
                        "heading level must be between 1 and 6",
                    ));
                }
                set_field_mode(&mut mode, FieldMode::Heading(level), &meta)
            } else if meta.path.is_ident("ordered_list") {
                set_field_mode(&mut mode, FieldMode::OrderedList, &meta)
            } else if meta.path.is_ident("unordered_list") {
                set_field_mode(&mut mode, FieldMode::UnorderedList, &meta)
            } else if meta.path.is_ident("skip") {
                skip = true;
                Ok(())
            } else if meta.path.is_ident("diff") {
                diff = true;
                if meta.input.peek(syn::token::Paren) {
                    meta.parse_nested_meta(|nested| {
                        if nested.path.is_ident("append") {
                            set_collection_diff_mode(
                                &mut collection_diff_mode,
                                CollectionDiffMode::Append,
                                &nested,
                            )
                        } else if nested.path.is_ident("set") {
                            set_collection_diff_mode(
                                &mut collection_diff_mode,
                                CollectionDiffMode::Set,
                                &nested,
                            )
                        } else if nested.path.is_ident("seq") {
                            set_collection_diff_mode(
                                &mut collection_diff_mode,
                                CollectionDiffMode::Seq,
                                &nested,
                            )
                        } else if nested.path.is_ident("key")
                            || nested.path.is_ident("key_attr")
                            || nested.path.is_ident("keyed")
                        {
                            let value = nested.value()?;
                            let value = value.parse::<LitStr>()?;
                            validate_xml_name_literal(&value, "invalid XML keyed attribute name")?;
                            set_collection_diff_mode(
                                &mut collection_diff_mode,
                                CollectionDiffMode::Keyed(value.value()),
                                &nested,
                            )
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
                set_collection_diff_mode(
                    &mut collection_diff_mode,
                    CollectionDiffMode::Append,
                    &meta,
                )
            } else if meta.path.is_ident("set") {
                set_collection_diff_mode(&mut collection_diff_mode, CollectionDiffMode::Set, &meta)
            } else if meta.path.is_ident("seq") {
                set_collection_diff_mode(&mut collection_diff_mode, CollectionDiffMode::Seq, &meta)
            } else if meta.path.is_ident("keyed") {
                let value = meta.value()?;
                let value = value.parse::<LitStr>()?;
                validate_xml_name_literal(&value, "invalid XML keyed attribute name")?;
                set_collection_diff_mode(
                    &mut collection_diff_mode,
                    CollectionDiffMode::Keyed(value.value()),
                    &meta,
                )
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

fn validate_xml_name_literal(value: &LitStr, message: &str) -> syn::Result<()> {
    let value_string = value.value();
    if is_valid_xml_name(&value_string) {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(value, message))
    }
}

fn validate_inferred_xml_name(
    value: &str,
    span: proc_macro2::Span,
    message: &str,
) -> syn::Result<()> {
    if is_valid_xml_name(value) {
        Ok(())
    } else {
        Err(syn::Error::new(span, message))
    }
}

fn is_valid_xml_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
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
        FieldMode::Root
        | FieldMode::Xml
        | FieldMode::CodeSpan
        | FieldMode::Paragraph
        | FieldMode::Heading(_)
        | FieldMode::OrderedList
        | FieldMode::UnorderedList => {
            unreachable!("new POM-only field mode reached legacy renderer")
        }
    }
}

fn pom_field_expr(
    receiver: proc_macro2::TokenStream,
    field_ident: &syn::Ident,
    field_name: &str,
    mode: FieldMode,
) -> proc_macro2::TokenStream {
    match mode {
        FieldMode::Default => quote! {
            ::agentview::agent_view::AgentViewValue::build_field(
                &#receiver.#field_ident,
                ::agentview::pom::XmlName::try_from(#field_name)?,
            )?
        },
        FieldMode::Attr => quote! {
            ::agentview::agent_view::ViewField::Attribute(
                ::agentview::pom::XmlAttribute::new(
                    ::agentview::pom::XmlName::try_from(#field_name)?,
                    ::agentview::agent_view::view_value(&#receiver.#field_ident),
                ),
            )
        },
        FieldMode::Element => quote! {
            {
                let mut child = ::agentview::pom::XmlNode::new(
                    ::agentview::pom::XmlName::try_from(#field_name)?,
                );
                child.push(::agentview::pom::MixedContent::text(
                    ::agentview::pom::TextNode::new(
                        ::agentview::agent_view::view_value(&#receiver.#field_ident),
                    ),
                ));
                ::agentview::agent_view::ViewField::Content(
                    ::agentview::pom::MixedContent::xml(child),
                )
            }
        },
        FieldMode::Text => quote! {
            ::agentview::agent_view::ViewField::Content(
                ::agentview::pom::MixedContent::text(
                    ::agentview::pom::TextNode::new(
                        ::agentview::agent_view::view_value(&#receiver.#field_ident),
                    ),
                ),
            )
        },
        FieldMode::Flatten => quote! {
            ::agentview::agent_view::ViewField::Children(
                ::agentview::agent_view::AgentViewValue::build_children(
                    &#receiver.#field_ident,
                )?,
            )
        },
        FieldMode::Root => quote! {
            ::agentview::agent_view::ViewField::Content(
                ::agentview::pom::MixedContent::xml(
                    ::agentview::agent_view::AgentView::build_root(
                        &#receiver.#field_ident,
                    )?,
                ),
            )
        },
        FieldMode::CodeSpan => quote! {
            {
                let mut child = ::agentview::pom::XmlNode::new(
                    ::agentview::pom::XmlName::try_from(#field_name)?,
                );
                child.push(::agentview::pom::MixedContent::markdown(
                    ::agentview::pom::MarkdownNode::CodeSpan(
                        ::agentview::pom::CodeSpanNode::new(
                            ::agentview::pom::TextNode::new(
                                ::agentview::agent_view::view_value(
                                    &#receiver.#field_ident,
                                ),
                            ),
                        ),
                    ),
                ));
                ::agentview::agent_view::ViewField::Content(
                    ::agentview::pom::MixedContent::xml(child),
                )
            }
        },
        FieldMode::Comment
        | FieldMode::Xml
        | FieldMode::Paragraph
        | FieldMode::Heading(_)
        | FieldMode::OrderedList
        | FieldMode::UnorderedList => {
            unreachable!("non-XML POM field mode reached XML renderer")
        }
    }
}

fn legacy_diff_strategy(
    replace: bool,
    collection_mode: Option<&CollectionDiffMode>,
) -> proc_macro2::TokenStream {
    if replace {
        return quote! { ::agentview::semantic_view::SemanticDiffStrategy::Replace };
    }
    match collection_mode {
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
            quote! { ::agentview::semantic_view::SemanticDiffStrategy::Keyed(#key_attr) }
        }
        None => quote! { ::agentview::semantic_view::SemanticDiffStrategy::Recursive },
    }
}

fn pom_diff_strategy(
    replace: bool,
    collection_mode: Option<&CollectionDiffMode>,
) -> proc_macro2::TokenStream {
    if replace {
        return quote! { ::agentview::pom::DiffStrategy::Replace };
    }
    match collection_mode {
        Some(CollectionDiffMode::Append) => {
            quote! { ::agentview::pom::DiffStrategy::Append }
        }
        Some(CollectionDiffMode::Set) => quote! { ::agentview::pom::DiffStrategy::Set },
        Some(CollectionDiffMode::Seq) => {
            quote! { ::agentview::pom::DiffStrategy::Sequence }
        }
        Some(CollectionDiffMode::Keyed(key_attr)) => {
            quote! {
                ::agentview::pom::DiffStrategy::Keyed(
                    ::agentview::pom::XmlName::try_from(#key_attr)?,
                )
            }
        }
        None => quote! { ::agentview::pom::DiffStrategy::Recursive },
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
