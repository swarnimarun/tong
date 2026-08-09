pub fn lib() -> u32 { 3 }
#[cfg(test)]
mod tests {
    #[test]
    fn unit() { assert_eq!(super::lib(), 3); }
    #[test]
    fn uses_dev_dep() { assert_eq!(super::lib() + crate::tool::tool(), 5); }
}
