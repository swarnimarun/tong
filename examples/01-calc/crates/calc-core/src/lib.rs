//! The calculator core: pure arithmetic.

/// Adds two numbers.
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

/// Subtracts two numbers.
pub fn sub(a: i64, b: i64) -> i64 {
    a - b
}

/// Multiplies two numbers.
pub fn mul(a: i64, b: i64) -> i64 {
    a * b
}

/// Divides two numbers; panics on zero (like the demo deserves).
pub fn div(a: i64, b: i64) -> i64 {
    a / b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_is_correct() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(sub(5, 3), 2);
        assert_eq!(mul(3, 4), 12);
        assert_eq!(div(12, 3), 4);
    }
}
