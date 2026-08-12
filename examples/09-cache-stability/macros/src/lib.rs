extern crate proc_macro;

use proc_macro::TokenStream;

#[proc_macro]
pub fn forty_two(_: TokenStream) -> TokenStream {
    "42u32".parse().expect("constant token stream")
}
