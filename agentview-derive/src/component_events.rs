use std::collections::{HashMap, HashSet};

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields};

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_events(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_events(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ident = input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            input.generics,
            "ComponentEvents enums cannot be generic",
        ));
    }

    let Data::Enum(data) = input.data else {
        return Err(syn::Error::new_spanned(
            ident,
            "ComponentEvents can only be derived for enums",
        ));
    };

    let variant_names = data
        .variants
        .iter()
        .map(|variant| variant.ident.to_string())
        .collect::<HashSet<_>>();
    let mut selector_names = HashMap::new();
    let mut selectors = Vec::new();
    for (index, variant) in data.variants.into_iter().enumerate() {
        if index > u16::MAX as usize {
            return Err(syn::Error::new_spanned(
                variant.ident,
                "ComponentEvents supports at most 65536 routes",
            ));
        }
        let variant_ident = variant.ident;
        let Fields::Unnamed(fields) = variant.fields else {
            return Err(syn::Error::new_spanned(
                variant_ident,
                "ComponentEvents variants must be single-field tuple variants",
            ));
        };
        if fields.unnamed.len() != 1 {
            return Err(syn::Error::new_spanned(
                fields,
                "ComponentEvents variants must carry exactly one event payload",
            ));
        }
        let payload = fields
            .unnamed
            .into_iter()
            .next()
            .expect("length checked")
            .ty;
        let selector_name = to_screaming_snake_case(&variant_ident.to_string());
        if let Some(previous) = selector_names.insert(selector_name.clone(), variant_ident.clone())
        {
            let mut error = syn::Error::new_spanned(
                &variant_ident,
                format!(
                    "ComponentEvents variants `{previous}` and `{variant_ident}` both generate selector `{selector_name}`"
                ),
            );
            error.combine(syn::Error::new_spanned(
                previous,
                format!("first `{selector_name}` selector generated here"),
            ));
            return Err(error);
        }
        if variant_names.contains(&selector_name) {
            return Err(syn::Error::new_spanned(
                &variant_ident,
                format!(
                    "generated ComponentEvents selector `{selector_name}` conflicts with an enum variant name"
                ),
            ));
        }
        let mut selector_ident = syn::parse_str::<syn::Ident>(&selector_name).map_err(|_| {
            syn::Error::new_spanned(
                &variant_ident,
                "ComponentEvents variant name cannot produce a valid SCREAMING_SNAKE_CASE selector",
            )
        })?;
        selector_ident.set_span(variant_ident.span());
        let variant_index = index as u16;

        selectors.push(quote! {
            pub const #selector_ident:
                ::agentview::component::authoring::__private::EventSelector<Self, #payload> =
                    ::agentview::component::authoring::__private::EventSelector::__component_events_v1(
                        ::std::concat!(
                            ::std::module_path!(),
                            "::",
                            ::std::stringify!(#ident),
                            "::",
                            ::std::stringify!(#variant_ident),
                        ),
                        #variant_index,
                        |event: &Self| {
                            match event {
                                Self::#variant_ident(value) => ::std::option::Option::Some(value),
                                _ => ::std::option::Option::None,
                            }
                        },
                    );
        });
    }

    Ok(quote! {
        impl #ident {
            #(#selectors)*
        }
    })
}

fn to_screaming_snake_case(input: &str) -> String {
    let input = input.strip_prefix("r#").unwrap_or(input);
    let characters = input.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(input.len());

    for (index, character) in characters.iter().copied().enumerate() {
        if character == '_' {
            if !output.ends_with('_') {
                output.push('_');
            }
            continue;
        }

        let previous = index.checked_sub(1).and_then(|index| characters.get(index));
        let next = characters.get(index + 1);
        let begins_word = character.is_uppercase()
            && previous.is_some_and(|previous| {
                *previous != '_'
                    && (previous.is_lowercase()
                        || previous.is_numeric()
                        || (previous.is_uppercase()
                            && next.is_some_and(|next| next.is_lowercase())))
            });
        if begins_word && !output.ends_with('_') {
            output.push('_');
        }
        output.extend(character.to_uppercase());
    }

    output
}

#[cfg(test)]
mod tests {
    use super::to_screaming_snake_case;

    #[test]
    fn selector_names_preserve_acronym_words() {
        assert_eq!(to_screaming_snake_case("Text"), "TEXT");
        assert_eq!(to_screaming_snake_case("ClockExpired"), "CLOCK_EXPIRED");
        assert_eq!(to_screaming_snake_case("HTTPRequest"), "HTTP_REQUEST");
        assert_eq!(to_screaming_snake_case("HTTP2Event"), "HTTP2_EVENT");
    }
}
