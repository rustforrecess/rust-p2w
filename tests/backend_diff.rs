//! Runs the SAME probe matrix through both backends and writes down where they
//! disagree.
//!
//! There are two of them, and they are different languages in places nobody
//! chose:
//!
//! * **WASM-GC** (`compile_to_wat`) — the browser IDE path. Values are
//!   `(ref null any)`, memory is the engine's problem, no linear memory.
//! * **Linear memory** (`compile_to_llvm_ir` + `runtime/`) — the Pico target,
//!   and also what the component/jco path runs. Reference counted, and it has
//!   `libm`, so it can do things the GC backend cannot.
//!
//! Every divergence found so far was found by ACCIDENT — integer overflow
//! behaved three different ways, `**` two, and a runtime trap printed a message
//! on one side and hung silently on the other. This finds them on purpose.
//!
//! The output is deliberately a list of DIFFERENCES, not a second full table.
//! A short list means convergence is cheap; a long one means we have been
//! maintaining two languages and should decide which is the real one.
//!
//! ## Running it
//!
//! ```text
//! P2W_DIFF=1 cargo test --test backend_diff -- --nocapture
//! P2W_DIFF=1 P2W_BLESS=1 cargo test --test backend_diff
//! ```
//!
//! It is OPT-IN because it shells out to clang and compiles a fresh binary per
//! probe — far too slow, and too toolchain-dependent, for the default suite.
//! Without `P2W_DIFF` it skips, and it also skips cleanly when clang or bash is
//! missing, the same way `tools/native_run.sh` does.

mod common;
use common::{Outcome, groups, probe};

use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Where cargo puts artifacts — this repo uses a shared target directory, so it
/// cannot be assumed to be `./target`.
fn target_dir() -> Option<PathBuf> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(manifest())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let key = "\"target_directory\":\"";
    let i = s.find(key)? + key.len();
    let j = s[i..].find('"')? + i;
    Some(PathBuf::from(s[i..j].replace("\\\\", "/")))
}

/// Build the runtime staticlib the native chain links against. Mirrors
/// `tools/native_run.sh` — and note the gotcha it encodes: it must be built as
/// a STATICLIB explicitly, or a stale artifact silently gets linked instead.
fn build_staticlib(target: &Path) -> Option<PathBuf> {
    let ok = Command::new("cargo")
        .args([
            "rustc",
            "--manifest-path",
            "runtime/Cargo.toml",
            "--release",
            "--crate-type",
            "staticlib",
            "--",
            "-C",
            "panic=abort",
        ])
        .current_dir(manifest())
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    for name in ["p2w_rt.lib", "libp2w_rt.a"] {
        let p = target.join("release").join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Compile one program through the LLVM/linear-memory path and run it.
///
/// A trap on that side spin-loops by design (it is device behaviour — there is
/// no process to exit on bare metal), so every run is capped. A timeout is
/// therefore reported as a trap, not as a failure of the harness.
fn probe_native(src: &str, dir: &Path, lib: &Path, n: usize) -> Outcome {
    let ir = match rust_p2w::compile_to_llvm_ir(src) {
        Ok(ir) => ir,
        Err(e) => return Outcome::CompileError(e),
    };
    let ll = dir.join(format!("p{n}.ll"));
    let exe = dir.join(format!("p{n}.exe"));
    if std::fs::write(&ll, ir).is_err() {
        return Outcome::CompileError("could not write IR".into());
    }
    let build = Command::new("clang")
        .arg("-Wno-override-module")
        .arg(&ll)
        .arg(dir.join("putc.c"))
        .arg(lib)
        .arg("-o")
        .arg(&exe)
        .output();
    match build {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            // A construct the native backend does not implement yet shows up
            // here as a link failure. That is itself a divergence worth seeing,
            // so report it rather than skipping the row.
            let err = String::from_utf8_lossy(&o.stderr);
            let first = err.lines().find(|l| l.contains("error")).unwrap_or("");
            return Outcome::CompileError(format!("native build failed: {}", first.trim()));
        }
        Err(e) => return Outcome::CompileError(format!("clang: {e}")),
    }

    // Spawn directly rather than going through `bash -c "timeout …"`: a Windows
    // path carries backslashes, which survive single quotes in bash and turn
    // every run into "command not found" — silently, and it looks exactly like
    // a universal divergence.
    let mut child = match Command::new(&exe)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Outcome::CompileError(format!("run: {e}")),
    };

    // A native trap spin-loops by design — there is no process to exit on bare
    // metal — so cap every run and report a hang as the trap it is.
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if started.elapsed() > std::time::Duration::from_secs(10) {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Outcome::CompileError(format!("wait: {e}")),
        }
    };
    let out = match child.wait_with_output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).replace('\r', ""),
        Err(_) => String::new(),
    };
    match status {
        Some(s) if s.success() => Outcome::Value(out.trim_end().to_string()),
        Some(_) => Outcome::Trap(out.trim().to_string()),
        None => Outcome::Trap(format!("{} [hung]", out.trim())),
    }
}

fn kind_of(o: &Outcome) -> &'static str {
    match o {
        Outcome::Value(_) => "value",
        Outcome::CompileError(_) => "compile error",
        Outcome::Trap(_) => "trap",
    }
}

fn text_of(o: &Outcome) -> String {
    let t = match o {
        Outcome::Value(v) | Outcome::CompileError(v) | Outcome::Trap(v) => v.clone(),
    };
    let t = t.replace('\n', " ⏎ ").replace('|', "\\|");
    if t.chars().count() > 90 {
        format!("{}…", t.chars().take(90).collect::<String>())
    } else if t.is_empty() {
        "(nothing)".into()
    } else {
        t
    }
}

/// Two outcomes agree if they are the same KIND and, for values, the same text.
///
/// Messages are deliberately NOT compared: the two backends word their errors
/// differently and always have. What matters is whether a program that works on
/// one works on the other, and whether a mistake is caught in the same place.
fn agree(a: &Outcome, b: &Outcome) -> bool {
    match (a, b) {
        (Outcome::Value(x), Outcome::Value(y)) => x == y,
        (Outcome::Trap(_), Outcome::Trap(_)) => true,
        (Outcome::CompileError(_), Outcome::CompileError(_)) => true,
        _ => false,
    }
}

#[test]
fn backends_agree_or_the_differences_are_written_down() {
    if std::env::var("P2W_DIFF").is_err() {
        eprintln!("SKIP: set P2W_DIFF=1 to run the two-backend comparison");
        return;
    }
    if !have("clang") || !have("bash") {
        eprintln!("SKIP: the native side needs clang and bash");
        return;
    }
    let Some(target) = target_dir() else {
        eprintln!("SKIP: could not locate the cargo target directory");
        return;
    };
    let Some(lib) = build_staticlib(&target) else {
        eprintln!("SKIP: runtime staticlib did not build");
        return;
    };

    let dir = target.join("backend-diff");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    // The byte sink the runtime links against, same as tools/native_run.sh.
    // Flush every byte. A native trap halts in a spin loop by design, so the
    // process never exits and a block-buffered stdout would take the trap's
    // message to the grave with it — the run would look silent when it was not.
    std::fs::write(
        dir.join("putc.c"),
        "#include <stdio.h>\n\
         void p2w_putc(unsigned char c) { putchar(c); fflush(stdout); }\n\
         int p2w_getc(void) { return getchar(); }\n\
         int p2w_host_seed(void) { return 42; }\n",
    )
    .expect("putc.c");

    let mut rows = Vec::new();
    let (mut total, mut differing) = (0usize, 0usize);
    let mut n = 0usize;

    for g in groups() {
        let mut group_rows = Vec::new();
        for pr in &g.probes {
            n += 1;
            total += 1;
            let gc = probe(pr.src);
            let native = probe_native(pr.src, &dir, &lib, n);
            if agree(&gc, &native) {
                continue;
            }
            differing += 1;
            group_rows.push(format!(
                "| `{}` | {} — `{}` | {} — `{}` |",
                pr.shown.replace('|', "\\|"),
                kind_of(&gc),
                text_of(&gc),
                kind_of(&native),
                text_of(&native)
            ));
        }
        if !group_rows.is_empty() {
            rows.push(format!(
                "\n### {}\n\n| program | WASM-GC | linear memory |\n|---|---|---|\n{}\n",
                g.title,
                group_rows.join("\n")
            ));
        }
    }

    let doc = format!(
        "# Where the two backends disagree\n\n\
         **Generated by `tests/backend_diff.rs` — do not edit by hand.** The same \
         probe matrix as `RUNTIME_SEMANTICS.md`, run through both backends.\n\n\
         Regenerate with `P2W_DIFF=1 P2W_BLESS=1 cargo test --test backend_diff`. \
         It is opt-in because it compiles and links a fresh native binary per \
         probe.\n\n\
         ## The two\n\n\
         - **WASM-GC** — `compile_to_wat`. The browser IDE path. No linear \
         memory; the engine owns memory.\n\
         - **Linear memory** — `compile_to_llvm_ir` + `runtime/`. The Pico \
         target, and what the component/jco path runs. Reference counted, and \
         it links `libm`.\n\n\
         Error WORDING is not compared — the two have always phrased things \
         differently. A row appears only when a program **works on one and not \
         the other**, or produces a **different value**.\n\n\
         ## Result\n\n\
         **{differing} of {total} probes disagree.**\n\n\
         A short list means converging the backends is cheap. A long one means \
         we have been maintaining two languages, and which one is the real \
         student-facing runtime is a decision rather than an accident.\n{}\n",
        if rows.is_empty() {
            "\nNo divergences in the current matrix.\n".to_string()
        } else {
            rows.join("")
        }
    );

    let path = manifest().join("BACKEND_DIVERGENCE.md");
    if std::env::var("P2W_BLESS").is_ok() {
        std::fs::write(&path, &doc).expect("write");
        eprintln!("blessed {} — {differing}/{total} differ", path.display());
    } else {
        print!("{doc}");
        eprintln!("{differing}/{total} probes disagree (P2W_BLESS=1 to record)");
    }
}
