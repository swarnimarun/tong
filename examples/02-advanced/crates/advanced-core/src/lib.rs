//! Advanced-core: compiled as both rlib and cdylib, with build-script
//! generated constants and config-driven cfg flags.

include!(concat!(env!("OUT_DIR"), "/message.rs"));

/// The greeting comes from the workspace [env] table.
pub fn greeting() -> &'static str {
    env!("APP_GREETING")
}

/// The message comes from the build script's OUT_DIR.
pub fn message() -> &'static str {
    MESSAGE
}

/// The message is also available as a compile-time env var.
pub fn message_env() -> &'static str {
    env!("ADVANCED_MESSAGE")
}

/// True when `.cargo/config.toml` rustflags were applied.
pub fn advanced_mode() -> bool {
    cfg!(advanced_mode)
}

/// True when the build script emitted a cfg.
pub fn has_build_script() -> bool {
    cfg!(has_build_script)
}

/// A simple exported function (also visible in the cdylib).
#[no_mangle]
pub extern "C" fn advanced_core_version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_constants_work() {
        assert_eq!(message(), "message from OUT_DIR");
        assert_eq!(message_env(), "message from OUT_DIR");
    }
}
