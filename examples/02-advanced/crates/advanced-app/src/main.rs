use advanced_core::{advanced_mode, greeting, has_build_script, message, message_env};
use shout::shout;

fn main() {
    println!("advanced-app (built by tong)");
    println!("  greeting:   {}", greeting());
    println!("  message:    {}", message());
    println!("  env message:{}", message_env());
    println!("  advanced_mode:  {}", advanced_mode());
    println!("  has_build_script: {}", has_build_script());
    println!("  shout:      {}", shout!("quiet please"));
    println!("  cdylib symbol available: advanced_core_version()");
}
