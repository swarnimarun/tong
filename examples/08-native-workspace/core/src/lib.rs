pub fn core() -> u32 { 1 }

#[cfg(feature = "extra")]
pub fn extra() -> u32 { extra::extra() }
