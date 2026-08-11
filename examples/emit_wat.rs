//! Read Python source on stdin, print the WASM-GC backend's WAT on stdout.
//! The browser-backend twin of `emit_ll` — used by benchmarks and by anyone
//! who wants to see the assembly their program becomes.
use std::io::Read;

fn main() {
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .expect("read stdin");
    match rust_p2w::compile_to_wat(&src) {
        Ok(wat) => print!("{wat}"),
        Err(e) => {
            eprintln!("compile error: {e}");
            std::process::exit(1);
        }
    }
}
