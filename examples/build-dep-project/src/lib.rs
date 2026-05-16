#[cfg(tong_build_dep)]
pub fn message() -> &'static str {
    "built with build-dep support"
}

#[cfg(not(tong_build_dep))]
pub fn message() -> &'static str {
    "built without build-dep support"
}
