//! The local member library: the app layer depends on it, so a change
//! here rebuilds only `web-core` + `web-app` — never the registry deps.

pub fn greeting() -> &'static str {
    "hello from tong + axum"
}
