//! Generates the feature table in `UPSTREAM_GAP.md` by COMPILING one program
//! per feature.
//!
//! The first version of that document was written from a grep over AST node
//! names and was wrong twice: it missed features that are **desugared** (a
//! lambda becomes a `def`, an f-string becomes concatenation, so neither has a
//! node to grep for), and it reported inheritance as missing because the probe
//! was `class B(A): pass` — which fails on `pass` in a class body, not on
//! inheritance.
//!
//! So the probes live here rather than in a scratch directory, and the table
//! regenerates from them. A hand-maintained list of what a compiler supports is
//! wrong the week after it is written.
//!
//! ```text
//! cargo test --test upstream_gap                 # check the table is current
//! P2W_BLESS=1 cargo test --test upstream_gap     # rewrite it
//! ```
//!
//! **A row moving from `no` to `yes` is a feature landing.** Read the diff.

use std::path::PathBuf;

/// (feature, program). The program must exercise ONLY the named feature —
/// anything else it trips over makes the row a lie.
fn probes() -> Vec<(&'static str, &'static str)> {
    vec![
        // --- statements the subset does not have ---
        (
            "with / context manager",
            "with open('f') as f:\n    print(1)\n",
        ),
        (
            "match / case",
            "x = 1\nmatch x:\n    case 1:\n        print('one')\n",
        ),
        ("walrus :=", "if (n := 5) > 3:\n    print(n)\n"),
        (
            "yield / generator function",
            "def g():\n    yield 1\nfor v in g():\n    print(v)\n",
        ),
        (
            "try / except",
            "try:\n    print(1)\nexcept:\n    print(2)\n",
        ),
        (
            "try / finally",
            "try:\n    print(1)\nfinally:\n    print(2)\n",
        ),
        ("raise", "raise ValueError('x')\n"),
        ("assert", "assert 1 == 1\n"),
        (
            "global",
            "x = 1\ndef f():\n    global x\n    x = 2\nf()\nprint(x)\n",
        ),
        (
            "nonlocal",
            "def o():\n    x = 1\n    def i():\n        nonlocal x\n        x = 2\n    i()\n    return x\nprint(o())\n",
        ),
        ("is / is not", "a = None\nprint(a is None)\n"),
        ("del name", "x = 1\ndel x\n"),
        (
            "for ... else",
            "for i in range(2):\n    print(i)\nelse:\n    print('done')\n",
        ),
        (
            "while ... else",
            "i = 0\nwhile i < 2:\n    i = i + 1\nelse:\n    print('done')\n",
        ),
        ("pass in a class body", "class A:\n    pass\nprint(1)\n"),
        // --- definitions ---
        (
            "decorator",
            "def d(f):\n    return f\n@d\ndef g():\n    return 1\nprint(g())\n",
        ),
        ("*args", "def f(*a):\n    return len(a)\nprint(f(1, 2))\n"),
        (
            "**kwargs",
            "def f(**k):\n    return len(k)\nprint(f(a=1))\n",
        ),
        (
            "multiple inheritance",
            "class A:\n    def f(self):\n        return 1\nclass B:\n    def g(self):\n        return 2\nclass C(A, B):\n    def h(self):\n        return 3\nprint(C().f())\n",
        ),
        (
            "from X import Y",
            "from math import sqrt\nprint(sqrt(4.0))\n",
        ),
        // --- ⭐ functions as values: one cause, many symptoms ---
        (
            "bind a function to a name",
            "def f(x):\n    return x\ng = f\nprint(g(1))\n",
        ),
        (
            "pass a function as an argument",
            "def apply(fn, v):\n    return fn(v)\ndef d(x):\n    return x * 2\nprint(apply(d, 3))\n",
        ),
        (
            "return a closure",
            "def mk():\n    x = 1\n    def i():\n        return x\n    return i\nprint(mk()())\n",
        ),
        ("map / filter", "print(list(map(abs, [-1])))\n"),
        ("sorted(key=...)", "print(sorted([[2], [1]], key=len))\n"),
        // --- values and builtins ---
        ("bytes literal", "b = b'hi'\nprint(len(b))\n"),
        ("integer past 2^31", "print(4000000000)\n"),
        ("percent formatting", "print('%d x' % 5)\n"),
        ("divmod", "print(divmod(7, 2))\n"),
        ("type()", "print(type(1))\n"),
        ("isinstance()", "print(isinstance(1, int))\n"),
        // --- present; here so a REGRESSION shows up as a row flipping ---
        (
            "inheritance",
            "class A:\n    def f(self):\n        return 1\nclass B(A):\n    def g(self):\n        return 2\nprint(B().f())\n",
        ),
        (
            "super()",
            "class A:\n    def __init__(self):\n        self.x = 1\nclass B(A):\n    def __init__(self):\n        super().__init__()\nprint(B().x)\n",
        ),
        ("lambda", "f = lambda x: x + 1\nprint(f(1))\n"),
        ("f-string", "n = 1\nprint(f'v={n}')\n"),
        ("generator expression", "print(sum(x for x in range(3)))\n"),
        (
            "closure capture",
            "def o():\n    x = 5\n    def i():\n        return x\n    return i()\nprint(o())\n",
        ),
        ("del item", "xs = [1, 2]\ndel xs[0]\nprint(xs)\n"),
        ("chained comparison", "x = 5\nprint(1 < x < 10)\n"),
        (
            "keyword args at a call",
            "def f(a, b):\n    return a + b\nprint(f(b=1, a=2))\n",
        ),
    ]
}

/// `__slots__` needs its own probe, because "it compiles" is the WRONG signal:
/// the class declaring it must then REFUSE a new attribute. It does not, so the
/// declaration is being read as an ordinary class variable.
fn slots_is_enforced() -> bool {
    rust_p2w::try_compile(
        "class A:\n    __slots__ = ('x',)\n    def __init__(self):\n        self.x = 1\na = A()\na.y = 2\nprint(a.y)\n",
    )
    .is_err()
}

fn render() -> String {
    let mut yes = Vec::new();
    let mut no = Vec::new();
    for (name, src) in probes() {
        match rust_p2w::try_compile(src) {
            Ok(_) => yes.push(name),
            Err(e) => no.push((name, e.message.replace('|', "\\|"))),
        }
    }

    let mut out = String::from(
        "# Feature probes\n\n\
         **Generated by `tests/upstream_gap.rs` — do not edit by hand.** Every row \
         is a program that was compiled, not a keyword that was searched for. A \
         hand-maintained list of what a compiler supports is wrong the week after \
         it is written, and the first version of this table was wrong on the day: \
         it missed features that are DESUGARED (a lambda becomes a `def`, so there \
         is no node to grep for) and reported inheritance as missing because the \
         probe used `pass` in a class body.\n\n\
         Regenerate with `P2W_BLESS=1 cargo test --test upstream_gap`. **A row \
         moving is a feature landing or a regression** — read the diff.\n\n\
         See `UPSTREAM_GAP.md` for what these gaps mean and which of upstream \
         p2w's designs are worth taking.\n\n",
    );

    out.push_str(&format!(
        "## Rejected ({})\n\n| feature | message |\n|---|---|\n",
        no.len()
    ));
    for (name, msg) in &no {
        let m = if msg.chars().count() > 80 {
            format!("{}…", msg.chars().take(80).collect::<String>())
        } else {
            msg.clone()
        };
        out.push_str(&format!("| `{name}` | {m} |\n"));
    }

    out.push_str(&format!("\n## Compiles ({})\n\n", yes.len()));
    for name in &yes {
        out.push_str(&format!("- `{name}`\n"));
    }

    out.push_str(&format!(
        "\n## `__slots__`\n\n\
         Declared slots are **{}** — a class that declares `__slots__` and then \
         takes an undeclared attribute {}. \"It compiles\" is the wrong signal \
         here; the right one is whether the extra attribute is REFUSED.\n",
        if slots_is_enforced() {
            "enforced"
        } else {
            "NOT enforced"
        },
        if slots_is_enforced() {
            "is correctly rejected"
        } else {
            "is accepted, so the declaration is being read as an ordinary class variable"
        }
    ));
    out
}

#[test]
fn the_feature_table_is_current() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("FEATURE_PROBES.md");
    let generated = render();
    if std::env::var("P2W_BLESS").is_ok() {
        std::fs::write(&path, &generated).expect("write");
        eprintln!("blessed {}", path.display());
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        committed.replace("\r\n", "\n"),
        generated,
        "FEATURE_PROBES.md is out of date — a feature landed, regressed, or a \
         message changed. Read the diff, then: P2W_BLESS=1 cargo test --test upstream_gap"
    );
}

#[test]
fn every_probe_exercises_something() {
    // A probe that compiles for the wrong reason is worse than no probe. This
    // catches the empty/whitespace mistake, not the subtle one — that is what
    // the "present" rows are for: they must keep compiling.
    for (name, src) in probes() {
        assert!(src.len() > 5, "{name}: probe is too short to test anything");
        assert!(src.ends_with('\n'), "{name}: probe must end with a newline");
    }
}
