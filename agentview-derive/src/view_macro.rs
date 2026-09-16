use std::collections::HashSet;

use proc_macro::TokenStream;
use quote::{quote, ToTokens};
use syn::{
    braced, parenthesized,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
    Attribute, Expr, ExprLit, GenericArgument, Ident, Lit, LitStr, Meta, Path, PathArguments,
    Result, Token, Type,
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
    ComponentProps(ComponentProps),
    XmlStreamingCall(XmlStreamingCall),
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
            let path = input.parse::<Path>()?;
            if !path_ends_with(&path, "XmlStreamingToolCall")
                && path
                    .segments
                    .iter()
                    .any(|segment| !matches!(segment.arguments, PathArguments::None))
            {
                return Err(syn::Error::new_spanned(
                    path,
                    "generic view root paths are supported only for XmlStreamingToolCall",
                ));
            }
            if input.peek(syn::token::Brace) {
                if is_markdown_path(&path, "paragraph") {
                    ViewNodeKind::MarkdownParagraph(MarkdownParagraph::parse_after_path(input)?)
                } else if path_ends_with(&path, "NativeToolCall")
                    || path_ends_with(&path, "CliCommand")
                    || path_ends_with(&path, "Action")
                {
                    ViewNodeKind::ComponentProps(ComponentProps::parse_after_path(path, input)?)
                } else if path_ends_with(&path, "XmlStreamingToolCall") {
                    if path
                        .segments
                        .last()
                        .is_some_and(|segment| matches!(segment.arguments, PathArguments::None))
                    {
                        ViewNodeKind::ComponentProps(ComponentProps::parse_after_path(path, input)?)
                    } else {
                        ViewNodeKind::XmlStreamingCall(XmlStreamingCall::parse_after_path(
                            path, input,
                        )?)
                    }
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
                ViewNodeKind::ComponentProps(component) => {
                    syn::Error::new_spanned(&component.path, "#[diff] can only annotate a POM root")
                }
                ViewNodeKind::XmlStreamingCall(component) => {
                    syn::Error::new_spanned(&component.path, "#[diff] can only annotate a POM root")
                }
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
            ViewNodeKind::ComponentProps(component) => component.expand(),
            ViewNodeKind::XmlStreamingCall(component) => component.expand(),
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

struct ComponentProps {
    path: Path,
    properties: Vec<(Ident, Expr)>,
}

impl ComponentProps {
    fn parse_after_path(path: Path, input: ParseStream<'_>) -> Result<Self> {
        let content;
        braced!(content in input);
        let mut properties = Vec::new();
        let mut property_names = HashSet::new();
        while !content.is_empty() {
            let name = content.parse::<Ident>()?;
            if !property_names.insert(identifier_text(&name)) {
                return Err(syn::Error::new_spanned(name, "duplicate Component prop"));
            }
            content.parse::<Token![:]>()?;
            properties.push((name, content.parse::<Expr>()?));
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        if path_ends_with(&path, "NativeToolCall") {
            for (name, _) in &properties {
                if !matches!(
                    identifier_text(name).as_str(),
                    "name" | "description" | "on_call"
                ) {
                    return Err(syn::Error::new_spanned(name, "unknown NativeToolCall prop"));
                }
            }
            for required in ["name", "on_call"] {
                if !property_names.contains(required) {
                    return Err(syn::Error::new_spanned(
                        &path,
                        format!("missing NativeToolCall prop `{required}`"),
                    ));
                }
            }
        } else if path_ends_with(&path, "Action") {
            for (name, _) in &properties {
                if !matches!(
                    identifier_text(name).as_str(),
                    "name" | "description" | "enabled" | "on_call"
                ) {
                    return Err(syn::Error::new_spanned(name, "unknown Action prop"));
                }
            }
            for required in ["name", "on_call"] {
                if !property_names.contains(required) {
                    return Err(syn::Error::new_spanned(
                        &path,
                        format!("missing Action prop `{required}`"),
                    ));
                }
            }
        } else if path_ends_with(&path, "CliCommand") {
            for (name, _) in &properties {
                if !matches!(
                    identifier_text(name).as_str(),
                    "command" | "enabled" | "on_call"
                ) {
                    return Err(syn::Error::new_spanned(name, "unknown CliCommand prop"));
                }
            }
            for required in ["command", "on_call"] {
                if !property_names.contains(required) {
                    return Err(syn::Error::new_spanned(
                        &path,
                        format!("missing CliCommand prop `{required}`"),
                    ));
                }
            }
        } else if path_ends_with(&path, "XmlStreamingToolCall") {
            for (name, _) in &properties {
                if !matches!(
                    identifier_text(name).as_str(),
                    "element"
                        | "description"
                        | "on_open"
                        | "on_delta"
                        | "on_complete"
                        | "on_invalid"
                ) {
                    return Err(syn::Error::new_spanned(
                        name,
                        "unknown XmlStreamingToolCall prop",
                    ));
                }
            }
            if !property_names.contains("element") {
                return Err(syn::Error::new_spanned(
                    &path,
                    "missing XmlStreamingToolCall prop `element`",
                ));
            }
            if !["on_open", "on_delta", "on_complete"]
                .iter()
                .any(|name| property_names.contains(*name))
            {
                return Err(syn::Error::new_spanned(
                    &path,
                    "XmlStreamingToolCall requires an `on_open`, `on_delta`, or `on_complete` callback",
                ));
            }
        }
        Ok(Self { path, properties })
    }

    fn expand(self) -> proc_macro2::TokenStream {
        let Self {
            path,
            mut properties,
        } = self;
        if path_ends_with(&path, "Action") {
            let name = take_property(&mut properties, "name").expect("required name");
            let description = take_property(&mut properties, "description")
                .map(|value| quote! { .description(#value) });
            let enabled =
                take_property(&mut properties, "enabled").map(|value| quote! { .enabled(#value) });
            let on_call = take_property(&mut properties, "on_call").expect("required on_call");
            return quote! {
                ::agentview::component::authoring::__private::dynamic_root(
                    #path::props().name(#name) #description #enabled .on_call(#on_call).build()
                )
            };
        }
        if path_ends_with(&path, "CliCommand") {
            // The command supplies the callback input type. Bind it before
            // either optional availability or the handler, in any prop order.
            let command = take_property(&mut properties, "command").expect("required command");
            let enabled =
                take_property(&mut properties, "enabled").map(|value| quote! { .enabled(#value) });
            let on_call = take_property(&mut properties, "on_call").expect("required on_call");
            return quote! {
                ::agentview::component::authoring::__private::dynamic_root(
                    #path::props().command(#command) #enabled .on_call(#on_call).build()
                )
            };
        }
        // Bind the element first so callbacks have a concrete input type even
        // when their props precede the element declaration in the view.
        let element = if path_ends_with(&path, "XmlStreamingToolCall") {
            let value = take_property(&mut properties, "element").expect("required element");
            Some(quote! { .element(#value) })
        } else {
            None
        };
        let setters = properties
            .into_iter()
            .map(|(name, value)| quote! { .#name(#value) });
        quote! {
            ::agentview::component::authoring::__private::dynamic_root(
                #path::props() #element #(#setters)* .build()
            )
        }
    }
}

struct XmlStreamingCall {
    path: Path,
    channels: Type,
    properties: Vec<(Ident, Expr)>,
    elements: Vec<XmlStreamingElement>,
}

impl XmlStreamingCall {
    fn parse_after_path(mut path: Path, input: ParseStream<'_>) -> Result<Self> {
        let last = path.segments.last_mut().expect("a parsed path has a leaf");
        let arguments = std::mem::replace(&mut last.arguments, PathArguments::None);
        let channels = match arguments {
            PathArguments::AngleBracketed(arguments) if arguments.args.len() == 1 => {
                match arguments.args.into_iter().next().expect("one argument") {
                    GenericArgument::Type(channels) => channels,
                    argument => {
                        return Err(syn::Error::new_spanned(
                            argument,
                            "expected a streaming channel type",
                        ));
                    }
                }
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &path,
                    "XmlStreamingToolCall requires one channel type: XmlStreamingToolCall::<Channels>",
                ));
            }
        };
        let content;
        braced!(content in input);
        let mut properties = Vec::new();
        let mut property_names = HashSet::new();
        let mut elements = Vec::new();
        while !content.is_empty() {
            let name = content.parse::<Ident>()?;
            if content.peek(syn::token::Brace) {
                if name != "XmlToolElement" {
                    return Err(syn::Error::new_spanned(
                        name,
                        "expected an XmlToolElement child",
                    ));
                }
                elements.push(XmlStreamingElement::parse_after_path(
                    syn::parse_quote!(#name),
                    &content,
                )?);
                if content.peek(Token![,]) {
                    content.parse::<Token![,]>()?;
                }
                continue;
            }
            let key = identifier_text(&name);
            if !matches!(
                key.as_str(),
                "identity"
                    | "version"
                    | "envelope"
                    | "allow_unclosed_text_at_eof"
                    | "ignore_unknown_elements"
                    | "state_with"
                    | "finish"
                    | "live_with"
                    | "without_live"
                    | "publish_with"
                    | "without_publication"
                    | "on_rejected"
            ) {
                return Err(syn::Error::new_spanned(
                    name,
                    "unknown XmlStreamingToolCall prop",
                ));
            }
            if !property_names.insert(key.clone()) {
                return Err(syn::Error::new_spanned(name, "duplicate Component prop"));
            }
            content.parse::<Token![:]>()?;
            let value = content.parse::<Expr>()?;
            if matches!(
                key.as_str(),
                "allow_unclosed_text_at_eof"
                    | "ignore_unknown_elements"
                    | "without_live"
                    | "without_publication"
            ) && !matches!(&value, Expr::Tuple(tuple) if tuple.elems.is_empty())
            {
                return Err(syn::Error::new_spanned(
                    value,
                    "this streaming prop expects `()`",
                ));
            }
            properties.push((name, value));
            if !content.is_empty() {
                content.parse::<Token![,]>()?;
            }
        }
        for required in ["identity", "state_with", "finish"] {
            if !property_names.contains(required) {
                return Err(syn::Error::new_spanned(
                    &path,
                    format!("missing XmlStreamingToolCall prop `{required}`"),
                ));
            }
        }
        for (with, without) in [
            ("live_with", "without_live"),
            ("publish_with", "without_publication"),
        ] {
            if property_names.contains(with) == property_names.contains(without) {
                return Err(syn::Error::new_spanned(
                    &path,
                    format!("choose exactly one of `{with}` and `{without}`"),
                ));
            }
        }
        if elements.is_empty() {
            return Err(syn::Error::new_spanned(
                &path,
                "XmlStreamingToolCall requires an XmlToolElement child",
            ));
        }
        Ok(Self {
            path,
            channels,
            properties,
            elements,
        })
    }

    fn expand(self) -> proc_macro2::TokenStream {
        let Self {
            path,
            channels,
            mut properties,
            elements,
        } = self;
        let identity = take_property(&mut properties, "identity").expect("required identity");
        let state = take_property(&mut properties, "state_with").expect("required state");
        let finish = take_property(&mut properties, "finish").expect("required finish");
        let elements = elements.into_iter().map(XmlStreamingElement::expand);
        let setters =
            properties
                .into_iter()
                .map(|(name, value)| match identifier_text(&name).as_str() {
                    "allow_unclosed_text_at_eof"
                    | "ignore_unknown_elements"
                    | "without_live"
                    | "without_publication" => quote! { .#name() },
                    _ => quote! { .#name(#value) },
                });
        quote! {
            #path::new::<#channels>(#identity)
                .state_with(#state)
                #(#elements)*
                .finish(#finish)
                #(#setters)*
                .build()
        }
    }
}

struct XmlStreamingElement {
    contract: Expr,
    open: Option<Expr>,
    delta: Option<Expr>,
    complete: XmlStreamingCompletion,
}

enum XmlStreamingCompletion {
    Complete(Expr),
    Validated { validate: Expr, reduce: Expr },
}

impl XmlStreamingElement {
    fn parse_after_path(path: Path, input: ParseStream<'_>) -> Result<Self> {
        let ComponentProps { mut properties, .. } =
            ComponentProps::parse_after_path(path.clone(), input)?;
        for (name, _) in &properties {
            if !matches!(
                identifier_text(name).as_str(),
                "contract" | "on_open" | "on_delta" | "on_complete" | "on_complete_validated"
            ) {
                return Err(syn::Error::new_spanned(name, "unknown XmlToolElement prop"));
            }
        }
        let contract = take_property(&mut properties, "contract").ok_or_else(|| {
            syn::Error::new_spanned(&path, "missing XmlToolElement prop `contract`")
        })?;
        let open = take_property(&mut properties, "on_open");
        let delta = take_property(&mut properties, "on_delta");
        let complete = match (
            take_property(&mut properties, "on_complete"),
            take_property(&mut properties, "on_complete_validated"),
        ) {
            (Some(complete), None) => XmlStreamingCompletion::Complete(complete),
            (None, Some(Expr::Tuple(tuple))) if tuple.elems.len() == 2 => {
                let mut elements = tuple.elems.into_iter();
                XmlStreamingCompletion::Validated {
                    validate: elements.next().expect("first tuple element"),
                    reduce: elements.next().expect("second tuple element"),
                }
            }
            (None, Some(value)) => {
                return Err(syn::Error::new_spanned(
                    value,
                    "on_complete_validated expects `(validate, reduce)`",
                ))
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    &path,
                    "choose exactly one of `on_complete` and `on_complete_validated`",
                ))
            }
        };
        Ok(Self {
            contract,
            open,
            delta,
            complete,
        })
    }

    fn expand(self) -> proc_macro2::TokenStream {
        let Self {
            contract,
            open,
            delta,
            complete,
        } = self;
        let open = open.map(|callback| quote! { .on_open(#callback) });
        let delta = delta.map(|callback| quote! { .on_delta(#callback) });
        let complete = match complete {
            XmlStreamingCompletion::Complete(callback) => quote! { .on_complete(#callback) },
            XmlStreamingCompletion::Validated { validate, reduce } => {
                quote! { .on_complete_validated(#validate, #reduce) }
            }
        };
        quote! { .element(#contract, |__agentview_element| __agentview_element #open #delta #complete) }
    }
}

fn take_property(properties: &mut Vec<(Ident, Expr)>, key: &str) -> Option<Expr> {
    properties
        .iter()
        .position(|(name, _)| identifier_text(name) == key)
        .map(|index| properties.remove(index).1)
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

fn path_ends_with(path: &Path, leaf: &str) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == leaf)
}

fn identifier_text(identifier: &Ident) -> String {
    let rendered = identifier.to_token_stream().to_string();
    rendered.strip_prefix("r#").unwrap_or(&rendered).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expansion(input: proc_macro2::TokenStream) -> Result<Expr> {
        let body = syn::parse2::<ViewBody>(input)?;
        syn::parse2(body.expand())
    }

    #[test]
    fn native_props_accept_typed_capturing_callbacks_and_literal_descriptions() {
        expansion(quote! {
            NativeToolCall {
                on_call: move |input: MoveInput| game.update(|state| state.play(&input.uci)),
                description: "Play a move using {uci}",
                name: "play",
            }
            agentview::component::prelude::NativeToolCall {
                name: "inspect",
                on_call: move |input: InspectInput| async move { inspect(input).await }
            }
        })
        .expect("both sync and async callbacks are ordinary Rust prop values");
    }

    #[test]
    fn cli_props_bind_command_and_availability_before_the_callback_in_any_order() {
        let command = quote! { command: MOVE };
        let enabled = quote! { enabled: game.with(GameState::can_move).expect("mounted state") };
        let on_call = quote! { on_call: move |input| game.update(|state| state.play(&input.uci)) };
        for properties in [
            quote! { #command, #enabled, #on_call },
            quote! { #command, #on_call, #enabled },
            quote! { #enabled, #command, #on_call },
            quote! { #enabled, #on_call, #command },
            quote! { #on_call, #command, #enabled },
            quote! { #on_call, #enabled, #command },
        ] {
            let body = syn::parse2::<ViewBody>(quote! {
                commands::CliCommand { #properties }
            })
            .expect("scoped CLI props allow any declaration order");
            let ViewNodeKind::ComponentProps(component) =
                body.nodes.into_iter().next().expect("one root").kind
            else {
                panic!("CLI declarations must use the props builder");
            };
            let expression =
                syn::parse2::<Expr>(component.expand()).expect("valid Rust expression");
            let Expr::Call(call) = &expression else {
                panic!("props builder must be wrapped in dynamic_root");
            };
            let mut receiver = call.args.first().expect("one root argument");
            let mut methods = Vec::new();
            while let Expr::MethodCall(call) = receiver {
                methods.push(call.method.to_string());
                receiver = &call.receiver;
            }
            methods.reverse();
            assert_eq!(methods, ["command", "enabled", "on_call", "build"]);
            assert_eq!(
                receiver.to_token_stream().to_string(),
                "commands :: CliCommand :: props ()"
            );
        }
        expansion(quote! {
            CliCommand {
                command: MOVE,
                on_call: move |input: MoveInput| game.update(|state| state.play(&input.uci)),
            }
        })
        .expect("availability is optional");
    }

    #[test]
    fn cli_props_reject_missing_unknown_duplicate_and_generic_declarations() {
        for (input, message) in [
            (
                quote! { CliCommand { command: MOVE } },
                "missing CliCommand prop `on_call`",
            ),
            (
                quote! { CliCommand { on_call: handler } },
                "missing CliCommand prop `command`",
            ),
            (
                quote! { CliCommand { command: MOVE, on_call: handler, extra: () } },
                "unknown CliCommand prop",
            ),
            (
                quote! { CliCommand { command: MOVE, on_call: handler, enabled: true, enabled: false } },
                "duplicate Component prop",
            ),
            (
                quote! { #[diff(slot = "actions")] CliCommand { command: MOVE, on_call: handler } },
                "#[diff] can only annotate a POM root",
            ),
            (
                quote! { commands::CliCommand::<MoveInput> { command: MOVE, on_call: handler } },
                "generic view root paths are supported only for XmlStreamingToolCall",
            ),
        ] {
            let error = expansion(input).err().expect("invalid CLI declaration");
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn action_props_accept_inline_typed_and_zero_argument_callbacks() {
        expansion(quote! {
            Action {
                on_call: move |input: MoveInput| game.update(|state| state.play(&input.uci)),
                enabled: game.with(GameState::can_move).unwrap(),
                name: "move",
                description: "Choose a current legal move",
            }
            agentview::component::prelude::Action {
                name: "undo",
                on_call: move || game.update(GameState::undo),
            }
        })
        .expect("inline actions accept typed and no-input callbacks");
    }

    #[test]
    fn action_props_require_name_and_callback_and_reject_unknown_fields() {
        for (input, message) in [
            (
                quote! { Action { name: "move" } },
                "missing Action prop `on_call`",
            ),
            (
                quote! { Action { on_call: handler } },
                "missing Action prop `name`",
            ),
            (
                quote! { Action { name: "move", on_call: handler, command: MOVE } },
                "unknown Action prop",
            ),
            (
                quote! { Action { name: "move", on_call: handler, name: "undo" } },
                "duplicate Component prop",
            ),
        ] {
            let error = expansion(input).err().expect("invalid action declaration");
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn xml_callback_props_bind_the_element_before_callbacks_for_input_inference() {
        let body = syn::parse2::<ViewBody>(quote! {
            XmlStreamingToolCall {
                on_complete: move |text| history.update(|entries| entries.push(text)),
                description: "Speak text using <say>...</say>",
                on_delta: move |text: String| async move { speaker.write(text).await },
                element: XmlToolElement::text("say"),
                on_invalid: move |diagnostic| feedback.update(|items| items.push(diagnostic)),
            }
        })
        .expect("plain callback declarations do not require channel generics");
        let ViewNodeKind::ComponentProps(component) =
            body.nodes.into_iter().next().expect("one root").kind
        else {
            panic!("plain XML callback declaration must use the props builder");
        };
        let expanded = component.expand();
        let mut expression = syn::parse2::<Expr>(expanded).expect("valid Rust expression");
        let Expr::Call(call) = &mut expression else {
            panic!("props builder must be wrapped in dynamic_root");
        };
        let mut receiver = call.args.first().expect("one root argument");
        let mut methods = Vec::new();
        while let Expr::MethodCall(call) = receiver {
            methods.push(call.method.to_string());
            receiver = &call.receiver;
        }
        methods.reverse();
        assert_eq!(
            methods,
            [
                "element",
                "on_complete",
                "description",
                "on_delta",
                "on_invalid",
                "build"
            ]
        );
    }

    #[test]
    fn xml_props_accept_reordered_callbacks_and_multiple_typed_elements() {
        expansion(quote! {
            XmlStreamingToolCall::<Channels> {
                finish: finish_attempt,
                without_publication: (),
                XmlToolElement {
                    on_complete: move |state, complete| update(state, complete),
                    contract: first_contract,
                    on_delta: move |state, delta| stream(state, delta),
                    on_open: opened,
                }
                identity: "example.actions",
                state_with: |_| Ok::<_, Error>(State::default()),
                XmlToolElement {
                    contract: second_contract,
                    on_complete_validated: (validate, complete),
                },
                without_live: (),
            }
        })
        .expect("props and child declarations are independent of builder method order");
    }

    #[test]
    fn ordinary_uppercase_xml_names_remain_xml() {
        let body = syn::parse2::<ViewBody>(quote! {
            Status { value: "ready", "Current status" }
        })
        .expect("ordinary XML names remain valid");
        assert!(matches!(body.nodes[0].kind, ViewNodeKind::Xml(_)));
    }

    #[test]
    fn component_props_reject_duplicate_names_and_diff_placement() {
        for (input, message) in [
            (
                quote! { NativeToolCall { name: "a", name: "b" } },
                "duplicate Component prop",
            ),
            (
                quote! { NativeToolCall { name: "a" } },
                "missing NativeToolCall prop `on_call`",
            ),
            (
                quote! { NativeToolCall { on_call: handler } },
                "missing NativeToolCall prop `name`",
            ),
            (
                quote! { NativeToolCall { name: "a", on_call: handler, extra: 1 } },
                "unknown NativeToolCall prop",
            ),
            (
                quote! { #[diff(slot = "actions")] NativeToolCall { name: "a", on_call: handler } },
                "#[diff] can only annotate a POM root",
            ),
        ] {
            let error = expansion(input)
                .err()
                .expect("invalid component declaration");
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn xml_callback_props_require_an_element_and_a_primary_lifecycle_handler() {
        for (input, message) in [
            (
                quote! { XmlStreamingToolCall {} },
                "missing XmlStreamingToolCall prop `element`",
            ),
            (
                quote! { XmlStreamingToolCall { element: action, on_invalid: report } },
                "XmlStreamingToolCall requires an `on_open`, `on_delta`, or `on_complete` callback",
            ),
            (
                quote! { XmlStreamingToolCall { element: action, on_delta: stream, on_delta: again } },
                "duplicate Component prop",
            ),
            (
                quote! { XmlStreamingToolCall { element: action, on_complete: completed, unknown: () } },
                "unknown XmlStreamingToolCall prop",
            ),
            (
                quote! { #[diff(slot = "actions")] XmlStreamingToolCall { element: action, on_complete: completed } },
                "#[diff] can only annotate a POM root",
            ),
        ] {
            let error = expansion(input)
                .err()
                .expect("invalid XML callback declaration");
            assert_eq!(error.to_string(), message);
        }
        for handler in [
            quote! { on_open: opened },
            quote! { on_delta: stream },
            quote! { on_complete: completed },
        ] {
            expansion(quote! { XmlStreamingToolCall { element: action, #handler } })
                .expect("each primary lifecycle handler can be declared independently");
        }
    }

    #[test]
    fn xml_props_require_channels_and_complete_lifecycle_configuration() {
        for (input, message) in [
            (
                quote! { XmlStreamingToolCall::<First, Second> {} },
                "XmlStreamingToolCall requires one channel type",
            ),
            (
                quote! {
                    XmlStreamingToolCall::<Channels> {
                        identity: "actions",
                        state_with: init,
                        finish: finish,
                        without_live: (),
                        without_publication: (),
                        XmlToolElement { contract: action, on_open: opened }
                    }
                },
                "choose exactly one of `on_complete` and `on_complete_validated`",
            ),
            (
                quote! {
                    XmlStreamingToolCall::<Channels> {
                        identity: "actions",
                        state_with: init,
                        finish: finish,
                        without_live: (),
                        live_with: live,
                        without_publication: (),
                        XmlToolElement { contract: action, on_complete: completed }
                    }
                },
                "choose exactly one of `live_with` and `without_live`",
            ),
        ] {
            let error = expansion(input)
                .err()
                .expect("incomplete streaming declaration");
            assert!(error.to_string().starts_with(message), "{error}");
        }
    }
}
