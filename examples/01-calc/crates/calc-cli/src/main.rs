use calc_core::{add, div, mul, sub};

fn main() {
    println!("calc-cli (built by tong)");
    println!("  2 + 3 = {}", add(2, 3));
    println!("  5 - 3 = {}", sub(5, 3));
    println!("  3 * 4 = {}", mul(3, 4));
    println!(" 12 / 3 = {}", div(12, 3));
}
