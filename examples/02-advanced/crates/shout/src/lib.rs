//! A function-like proc macro that uppercases its string literal at
//! compile time.

use proc_macro::TokenStream;

/// Expands `shout!("hi")` into the string literal `"HI"`.
#[proc_macro]
pub fn shout(input: TokenStream) -> TokenStream {
    let text = input.to_string();
    let literal = text.trim();
    if !(literal.starts_with('"') && literal.ends_with('"')) {
        panic!("shout! expects a string literal, got {literal}");
    }
    let inner = &literal[1..literal.len() - 1];
    format!("\"{}\"", inner.to_uppercase()).parse().unwrap()
}
