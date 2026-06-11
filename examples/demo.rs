//! Quick demo: compile a sample Python program to WAT and print it.
//! Run with: cargo run --example demo

fn main() {
    let src = "\
# A first taste of rust-p2w
x = 6
y = 7
print(\"product:\", x * y)
total = x + y
print(\"sum:\", total)
";
    println!("--- Python ---\n{src}");
    match rust_p2w::compile_to_wat(src) {
        Ok(wat) => println!("--- WAT ---\n{wat}"),
        Err(e) => println!("--- error ---\n{e}"),
    }
}
