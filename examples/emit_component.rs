//! The component converter's CLI face (LESSON_PLAYER.md step 5e).
//!
//! Reads a program on stdin; args are `<instance> <api,csv> <outdir>`. Writes
//! the converter's three generated inputs into `<outdir>`:
//! `component.py` (the verbatim def group), `component.wit` (the world), and
//! `shim.c` (the canonical-ABI shim). `tools/componentize.sh` drives this,
//! then builds the actual component with clang + wasm-tools.
use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: emit_component <instance> <api,csv> <outdir>  (source on stdin)");
        std::process::exit(1);
    }
    let instance = &args[1];
    let api: Vec<String> = args[2].split(',').map(str::to_string).collect();
    let outdir = std::path::Path::new(&args[3]);

    let mut src = String::new();
    std::io::stdin()
        .read_to_string(&mut src)
        .expect("read stdin");

    match rust_p2w::to_component(&src, instance, &api) {
        Ok(x) => {
            std::fs::create_dir_all(outdir).expect("create outdir");
            std::fs::write(outdir.join("component.py"), &x.python).expect("write py");
            std::fs::write(outdir.join("component.wit"), &x.wit).expect("write wit");
            std::fs::write(outdir.join("shim.c"), &x.shim_c).expect("write shim");
            // The host's event-wiring manifest (5e-c): (selector, event,
            // handler-export) rows the host installs DOM listeners from.
            std::fs::write(outdir.join("wiring.json"), x.wiring_json()).expect("write wiring");
            println!(
                "exports: {}",
                x.exports
                    .iter()
                    .map(|e| e.api_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("imports: {}", x.imports.join(", "));
            for w in &x.wiring {
                println!("wire: {} {} -> {}", w.selector, w.event, w.handler);
            }
            for (name, why) in &x.skipped {
                println!("internal: {name} ({why})");
            }
            // The def names clang must keep alive (`-Wl,--export=` each).
            println!(
                "keep: {}",
                x.exports
                    .iter()
                    .map(|e| e.def_name.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
        Err(e) => {
            eprintln!("not convertible: {e}");
            std::process::exit(1);
        }
    }
}
