//! Read Python source on stdin, print the native-backend LLVM IR on stdout.
//! Used by tools/native_run.sh to drive the host run-oracle.
use std::io::Read;

fn main() {
    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .expect("read stdin");
    match rust_p2w::compile_to_llvm_ir(&src) {
        Ok(ir) => print!("{ir}"),
        Err(e) => {
            eprintln!("compile error: {e}");
            std::process::exit(1);
        }
    }
}
