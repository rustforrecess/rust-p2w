//! Measure emitted WAT sizes for representative programs.
//! Writes each program's WAT to target/sizes/<name>.wat so we can assemble
//! them to real .wasm and compare. Run: cargo run --example sizes
use std::fs;

fn main() {
    let progs: &[(&str, &str)] = &[
        ("empty", ""),
        ("hello", "print(\"hello world\")"),
        (
            "arith",
            "x = 6\ny = 7\nprint(\"product:\", x * y)\ntotal = x + y\nprint(\"sum:\", total)\n",
        ),
        (
            "loop",
            "total = 0\nfor i in range(10):\n    total = total + i\nprint(total)\n",
        ),
        (
            "func",
            "def fib(n):\n    if n < 2:\n        return n\n    return fib(n-1) + fib(n-2)\nprint(fib(10))\n",
        ),
        (
            "containers",
            "xs = [1, 2, 3]\nxs.append(4)\nd = {\"a\": 1, \"b\": 2}\nfor x in xs:\n    print(x)\nprint(d[\"a\"], len(xs))\n",
        ),
    ];
    let dir = "target/sizes";
    fs::create_dir_all(dir).unwrap();
    println!("{:<12} {:>10}", "program", "wat bytes");
    for (name, src) in progs {
        match rust_p2w::compile_to_wat(src) {
            Ok(wat) => {
                fs::write(format!("{dir}/{name}.wat"), &wat).unwrap();
                println!("{name:<12} {:>10}", wat.len());
            }
            Err(e) => println!("{name:<12} ERROR: {e}"),
        }
    }
}
