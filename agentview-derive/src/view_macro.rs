use std::collections::HashSet;

use proc_macro::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    braced, parenthesized,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    Attribute, Expr, ExprLit, Ident, Lit, LitStr, Meta, Path, Result, Token,
};

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let body = parse_macro_input!(input as ViewBody);
    body.expand().into()
}

struct ViewBody {
    nodes: Vec<ViewNode>,
}

impl Parse for ViewBody {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut nodes = Vec::new();
        while !input.is_empty() {
            nodes.push(ViewNode::parse(input)?);
        }
        Ok(Self { nodes })
    }
}

impl ViewBody {
    fn expand(self) -> proc_macro2::TokenStream {
        let nodes = self.nodes.into_iter().map(ViewNode::expand);
        quote! {
            ::agentview::component::authoring::__private::fragment(
                ::std::vec![#(#nodes),*]
            )
        }
    }
}

#[derive(Clone, Copy)]
enum Placement {
    SystemOnce,
    Developer,
    DeveloperRepeat,
    User,
    UserRepeat,
    Assistant,
}

struct ViewNode {
    directives: ViewDirectives,
    kind: ViewNodeKind,
}

#[derive(Default)]
struct ViewDirectives {
    placement: Option<Placement>,
    diff_slot: Option<LitStr>,
}

enum ViewNodeKind {
    Paragraph(TextLiteral),
    MarkdownParagraph(MarkdownParagraph),
    Xml(XmlElement),
    Call(Expr),
    Dynamic(Expr),
}

impl ViewNode {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let directives = parse_directives(input.call(Attribute::parse_outer)?)?;
        let kind = if input.peek(LitStr) {
            let literal = input.parse::<LitStr>()?;
            let literal = TextLiteral::new(literal, true)?;
            ViewNodeKind::Paragraph(literal)
        } else if input.peek(syn::token::Brace) {
            let content;
            braced!(content in input);
            let expression = content.parse::<Expr>()?;
            if !content.is_empty() {
                return Err(content.error("dynamic view root expects exactly one Rust expression"));
            }
            ViewNodeKind::Dynamic(expression)
        } else {
            let path = input.call(Path::parse_mod_style)?;
            if input.peek(syn::token::Brace) {
                if is_markdown_path(&path, "paragraph") {
                    ViewNodeKind::MarkdownParagraph(MarkdownParagraph::parse_after_path(input)?)
                } else {
                    ViewNodeKind::Xml(XmlElement::parse_after_path(path, input)?)
                }
            } else if input.peek(syn::token::Paren) {
                let content;
                parenthesized!(content in input);
                let arguments = Punctuated::<Expr, Token![,]>::parse_terminated(&content)?;
                let call: Expr = syn::parse_quote!(#path(#arguments));
                ViewNodeKind::Call(call)
            } else {
                return Err(input.error("expected an XML element or Component call"));
            }
        };
        if directives.diff_slot.is_some()
            && !matches!(
                &kind,
                ViewNodeKind::Paragraph(_)
                    | ViewNodeKind::MarkdownParagraph(_)
                    | ViewNodeKind::Xml(_)
                    | ViewNodeKind::Dynamic(_)
            )
        {
            let error = match &kind {
                ViewNodeKind::Call(expression) => {
                    syn::Error::new_spanned(expression, "#[diff] can only annotate a POM root")
                }
                ViewNodeKind::Paragraph(_)
                | ViewNodeKind::MarkdownParagraph(_)
                | ViewNodeKind::Xml(_)
                | ViewNodeKind::Dynamic(_) => unreachable!("POM roots were accepted above"),
            };
            return Err(error);
        }
        Ok(Self { directives, kind })
    }

    fn expand(self) -> proc_macro2::TokenStream {
        let is_diff = self.directives.diff_slot.is_some();
        let node = match self.kind {
            ViewNodeKind::Paragraph(literal) => quote! {
                ::agentview::component::authoring::__private::paragraph(#literal)
            },
            ViewNodeKind::MarkdownParagraph(paragraph) => paragraph.expand_component(),
            ViewNodeKind::Xml(element) => element.expand_component(),
            ViewNodeKind::Call(call) => quote! { #call },
            ViewNodeKind::Dynamic(expression) if is_diff => quote! {
                ::agentview::component::authoring::__private::dynamic_diff_root(#expression)
            },
            ViewNodeKind::Dynamic(expression) => quote! {
                ::agentview::component::authoring::__private::dynamic_root(#expression)
            },
        };
        let node = match self.directives.diff_slot {
            Some(slot) => quote! {
                ::agentview::component::authoring::__private::diff(#slot, #node)
            },
            None => node,
        };
        match self.directives.placement {
            Some(Placement::SystemOnce) => quote! {
                ::agentview::component::authoring::__private::system_once(
                    ::std::concat!(
                        "agentview::system_once:",
                        ::std::module_path!(),
                        ":",
                        ::std::line!(),
                        ":",
                        ::std::column!(),
                    ),
                    move || #node,
                )
            },
            Some(Placement::Developer) => quote! {
                ::agentview::component::authoring::__private::placed(
                    ::agentview::component::authoring::__private::Placement::Developer,
                    #node,
                )
            },
            Some(Placement::DeveloperRepeat) => quote! {
                ::agentview::component::authoring::__private::placed(
                    ::agentview::component::authoring::__private::Placement::DeveloperRepeat,
                    #node,
                )
            },
            Some(Placement::User) => quote! {
                ::agentview::component::authoring::__private::placed(
                    ::agentview::component::authoring::__private::Placement::User,
                    #node,
                )
            },
            Some(Placement::UserRepeat) => quote! {
                ::agentview::component::authoring::__private::placed(
                    ::agentview::component::authoring::__private::Placement::UserRepeat,
                    #node,
                )
            },
            Some(Placement::Assistant) => quote! {
                ::agentview::component::authoring::__private::placed(
                    ::agentview::component::authoring::__private::Placement::Assistant,
                    #node,
                )
            },
            None => node,
        }
    }
}

struct MarkdownParagraph {
    children: Vec<MarkdownInline>,
}

enum MarkdownInline {
    Text(TextLiteral),
    CodeSpan(TextLiteral),
}

impl MarkdownParagraph {
    fn parse_after_path(input: ParseStream<'_>) -> Result<Self> {
        let content;
        braced!(content in input);
        let mut children = Vec::new();
        while !content.is_empty() {
            if content.peek(LitStr) {
                let literal = content.parse::<LitStr>()?;
                children.push(MarkdownInline::Text(TextLiteral::new(literal, false)?));
                continue;
            }

            let path = content.call(Path::parse_mod_style)?;
            if !is_markdown_path(&path, "code_span") {
                return Err(syn::Error::new_spanned(
                    path,
                    "the minimal Markdown lowering supports only md::code_span inside md::paragraph",
                ));
            }
            let code_content;
            braced!(code_content in content);
            let literal = code_content.parse::<LitStr>()?;
            if !code_content.is_empty() {
                return Err(code_content.error("md::code_span expects exactly one text literal"));
            }
            children.push(MarkdownInline::CodeSpan(TextLiteral::new(literal, false)?));
        }
        Ok(Self { children })
    }

    fn expand_component(self) -> proc_macro2::TokenStream {
        let children = self.children.into_iter().map(MarkdownInline::expand);
        quote! {
            ::agentview::component::authoring::__private::markdown_paragraph(
                ::std::vec![#(#children),*]
            )
        }
    }
}

impl MarkdownInline {
    fn expand(self) -> proc_macro2::TokenStream {
        match self {
            Self::Text(text) => quote! {
                ::agentview::component::authoring::__private::markdown_text(#text)
            },
            Self::CodeSpan(text) => quote! {
                ::agentview::component::authoring::__private::markdown_code_span(#text)
            },
        }
    }
}

fn parse_directives(attributes: Vec<Attribute>) -> Result<ViewDirectives> {
    let mut directives = ViewDirectives::default();
    for attribute in attributes {
        let placement = if attribute.path().is_ident("system_once") {
            Some(parse_plain_placement(&attribute, Placement::SystemOnce)?)
        } else if attribute.path().is_ident("developer") {
            Some(parse_repeat_placement(
                &attribute,
                Placement::Developer,
                Placement::DeveloperRepeat,
            )?)
        } else if attribute.path().is_ident("user") {
            Some(parse_repeat_placement(
                &attribute,
                Placement::User,
                Placement::UserRepeat,
            )?)
        } else if attribute.path().is_ident("assistant") {
            Some(parse_plain_placement(&attribute, Placement::Assistant)?)
        } else {
            None
        };
        if let Some(placement) = placement {
            if directives.placement.replace(placement).is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "a view root accepts at most one placement directive",
                ));
            }
            continue;
        }

        if attribute.path().is_ident("diff") {
            if directives.diff_slot.is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "a view root accepts at most one diff directive",
                ));
            }
            let mut slot = None;
            attribute.parse_nested_meta(|meta| {
                if !meta.path.is_ident("slot") {
                    return Err(meta.error("#[diff] accepts only `slot = \"...\"`"));
                }
                if slot.is_some() {
                    return Err(meta.error("#[diff] accepts exactly one slot"));
                }
                slot = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            })?;
            let slot = slot.ok_or_else(|| {
                syn::Error::new_spanned(&attribute, "#[diff] requires `slot = \"...\"`")
            })?;
            if slot.value().is_empty() {
                return Err(syn::Error::new_spanned(
                    slot,
                    "#[diff] slot cannot be empty",
                ));
            }
            directives.diff_slot = Some(slot);
            continue;
        }

        return Err(syn::Error::new_spanned(
            attribute,
            "view roots support #[system_once], #[developer], #[developer(repeat)], #[user], #[user(repeat)], #[assistant], and #[diff(slot = \"...\")]",
        ));
    }
    Ok(directives)
}

fn parse_plain_placement(attribute: &Attribute, placement: Placement) -> Result<Placement> {
    if matches!(&attribute.meta, Meta::Path(_)) {
        Ok(placement)
    } else {
        Err(syn::Error::new_spanned(
            attribute,
            "prompt placement directives do not accept arguments",
        ))
    }
}

fn parse_repeat_placement(
    attribute: &Attribute,
    ordinary: Placement,
    repeat: Placement,
) -> Result<Placement> {
    if matches!(&attribute.meta, Meta::Path(_)) {
        return Ok(ordinary);
    }
    if !matches!(&attribute.meta, Meta::List(_)) {
        return Err(syn::Error::new_spanned(
            attribute,
            "placement accepts only the `repeat` argument",
        ));
    }

    let mut repeat_seen = false;
    attribute.parse_nested_meta(|meta| {
        if !meta.path.is_ident("repeat") {
            return Err(meta.error("placement accepts only the `repeat` argument"));
        }
        if meta.input.peek(syn::Token![=]) || meta.input.peek(syn::token::Paren) {
            return Err(meta.error("the `repeat` placement argument does not accept a value"));
        }
        if std::mem::replace(&mut repeat_seen, true) {
            return Err(meta.error("the `repeat` placement argument may occur only once"));
        }
        Ok(())
    })?;
    if !repeat_seen {
        return Err(syn::Error::new_spanned(
            attribute,
            "placement requires the `repeat` argument",
        ));
    }
    Ok(repeat)
}

struct XmlElement {
    name: Ident,
    attributes: Vec<(Ident, AttributeValue)>,
    children: Vec<XmlChild>,
}

enum XmlChild {
    Text(TextLiteral),
    Element(XmlElement),
}

impl XmlElement {
    fn parse_after_path(path: Path, input: ParseStream<'_>) -> Result<Self> {
        if path.leading_colon.is_some() || path.segments.len() != 1 {
            return Err(syn::Error::new_spanned(
                path,
                "the minimal view lowering supports ordinary XML names only",
            ));
        }
        let name = path.segments[0].ident.clone();
        let content;
        braced!(content in input);
        let mut attributes = Vec::new();
        let mut attribute_names = HashSet::new();
        let mut children = Vec::new();
        let mut saw_child = false;

        while !content.is_empty() {
            if content.peek(LitStr) {
                let literal = content.parse::<LitStr>()?;
                let literal = TextLiteral::new(literal, false)?;
                saw_child = true;
                children.push(XmlChild::Text(literal));
                continue;
            }

            let child_name = content.parse::<Ident>()?;
            if content.peek(Token![:]) {
                if saw_child {
                    return Err(syn::Error::new_spanned(
                        child_name,
                        "XML attributes must appear before child content",
                    ));
                }
                content.parse::<Token![:]>()?;
                let value = content.parse::<Expr>()?;
                content.parse::<Token![,]>()?;
                let value = AttributeValue::new(value)?;
                let key = child_name.to_string();
                if !attribute_names.insert(key) {
                    return Err(syn::Error::new_spanned(
                        child_name,
                        "duplicate XML attribute",
                    ));
                }
                attributes.push((child_name, value));
                continue;
            }

            let child_path: Path = syn::parse_quote!(#child_name);
            saw_child = true;
            children.push(XmlChild::Element(Self::parse_after_path(
                child_path, &content,
            )?));
        }

        Ok(Self {
            name,
            attributes,
            children,
        })
    }

    fn expand_component(self) -> proc_macro2::TokenStream {
        let (name, attributes, children) = self.expand_parts();
        quote! {
            ::agentview::component::authoring::__private::xml_component(
                #name,
                #attributes,
                #children,
            )
        }
    }

    fn expand_child(self) -> proc_macro2::TokenStream {
        let (name, attributes, children) = self.expand_parts();
        quote! {
            ::agentview::component::authoring::__private::xml_child(
                #name,
                #attributes,
                #children,
            )
        }
    }

    fn expand_parts(self) -> (LitStr, proc_macro2::TokenStream, proc_macro2::TokenStream) {
        let name = LitStr::new(&identifier_text(&self.name), self.name.span());
        let attributes = self.attributes.into_iter().map(|(name, value)| {
            let name = LitStr::new(&identifier_text(&name), name.span());
            quote! { (#name, #value) }
        });
        let children = self.children.into_iter().map(|child| match child {
            XmlChild::Text(text) => quote! {
                ::agentview::component::authoring::__private::xml_text(#text)
            },
            XmlChild::Element(element) => element.expand_child(),
        });
        (
            name,
            quote! { ::std::vec![#(#attributes),*] },
            quote! { ::std::vec![#(#children),*] },
        )
    }
}

struct TextLiteral {
    segments: Vec<LitStr>,
    slots: Vec<FormatSlot>,
}

struct FormatSlot {
    identifier: Ident,
    format: LitStr,
}

impl TextLiteral {
    fn new(literal: LitStr, root: bool) -> Result<Self> {
        validate_text_literal(&literal, root)?;
        let (segments, slots) = parse_text_template(&literal)?;
        Ok(Self { segments, slots })
    }
}

impl ToTokens for TextLiteral {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let mut parts = Vec::with_capacity(self.segments.len() + self.slots.len());
        for (index, segment) in self.segments.iter().enumerate() {
            parts.push(quote! {
                ::agentview::component::authoring::__private::static_text(#segment)
            });
            if let Some(slot) = self.slots.get(index) {
                let identifier = &slot.identifier;
                let format = &slot.format;
                parts.push(quote! {
                    ::agentview::component::authoring::__private::formatted_text_slot(
                        &#identifier,
                        ::std::format_args!(#format),
                    )
                });
            }
        }
        let expanded = quote! {
            ::agentview::component::authoring::__private::text_template(
                ::std::vec![#(#parts),*]
            )
        };
        tokens.extend(expanded);
    }
}

enum AttributeValue {
    Literal(TextLiteral),
    Dynamic(Expr),
}

impl AttributeValue {
    fn new(value: Expr) -> Result<Self> {
        if let Expr::Lit(ExprLit {
            lit: Lit::Str(literal),
            ..
        }) = value
        {
            return Ok(Self::Literal(TextLiteral::new(literal, false)?));
        }
        Ok(Self::Dynamic(value))
    }
}

impl ToTokens for AttributeValue {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let expanded = match self {
            Self::Literal(literal) => quote! { #literal },
            Self::Dynamic(value) => quote! {
                {
                    let __agentview_attribute_value = (#value);
                    ::agentview::component::authoring::__private::text_template(
                        ::std::vec![
                            ::agentview::component::authoring::__private::formatted_text_slot(
                                &__agentview_attribute_value,
                                ::std::format_args!("{}", __agentview_attribute_value),
                            )
                        ]
                    )
                }
            },
        };
        tokens.extend(expanded);
    }
}

fn parse_text_template(literal: &LitStr) -> Result<(Vec<LitStr>, Vec<FormatSlot>)> {
    let value = literal.value();
    let mut characters = value.chars().peekable();
    let mut current = String::new();
    let mut segments = Vec::new();
    let mut slots = Vec::new();

    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                current.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                current.push('}');
            }
            '{' => {
                segments.push(LitStr::new(&current, literal.span()));
                current.clear();
                let mut body = String::new();
                loop {
                    match characters.next() {
                        Some('}') => break,
                        Some('{') => {
                            return Err(syn::Error::new_spanned(
                                literal,
                                "nested braces are not supported in a formatted text slot",
                            ));
                        }
                        Some(character) => body.push(character),
                        None => {
                            return Err(syn::Error::new_spanned(
                                literal,
                                "formatted text slot is missing a closing brace",
                            ));
                        }
                    }
                }
                slots.push(parse_format_slot(literal, &body)?);
            }
            '}' => {
                return Err(syn::Error::new_spanned(
                    literal,
                    "unmatched closing brace in formatted text",
                ));
            }
            character => current.push(character),
        }
    }

    segments.push(LitStr::new(&current, literal.span()));
    Ok((segments, slots))
}

fn parse_format_slot(literal: &LitStr, body: &str) -> Result<FormatSlot> {
    let identifier = body.split_once(':').map_or(body, |(name, _)| name);
    let mut identifier = syn::parse_str::<Ident>(identifier).map_err(|_| {
        syn::Error::new_spanned(
            literal,
            "formatted text slots require a captured Rust identifier",
        )
    })?;
    identifier.set_span(literal.span());
    Ok(FormatSlot {
        identifier,
        format: LitStr::new(&format!("{{{body}}}"), literal.span()),
    })
}

fn validate_text_literal(literal: &LitStr, root: bool) -> Result<()> {
    let value = literal.value();
    if value.contains('\n') || value.contains('\r') {
        return Err(syn::Error::new_spanned(
            literal,
            if root {
                "multiline root text is not implemented by the minimal lowering"
            } else {
                "multiline XML text is not implemented by the minimal lowering"
            },
        ));
    }
    Ok(())
}

fn is_markdown_path(path: &Path, leaf: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 2
        && path.segments[0].ident == "md"
        && path.segments[1].ident == leaf
}

fn identifier_text(identifier: &Ident) -> String {
    let rendered = identifier.to_token_stream().to_string();
    rendered.strip_prefix("r#").unwrap_or(&rendered).to_owned()
}
