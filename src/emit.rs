//! WAT emission: an indented line buffer plus module/function builders.
//!
//! Replaces ad-hoc `format!` string concatenation so codegen composes
//! structurally — bodies nest into bodies, functions collect locals, and the
//! module renders its sections in order. This is the shape the boxed WASM-GC
//! backend needs (type section, globals, many helper functions); the typed
//! backend uses the same builders with most sections empty.

const INDENT: &str = "  ";

/// A buffer of WAT lines, each at a relative indent depth.
#[derive(Debug, Clone, Default)]
pub struct Body {
    lines: Vec<(usize, String)>,
}

impl Body {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a line at the body's base depth.
    pub fn push(&mut self, text: impl Into<String>) {
        self.lines.push((0, text.into()));
    }

    /// Push a line `depth` levels deeper than the body's base.
    pub fn push_in(&mut self, depth: usize, text: impl Into<String>) {
        self.lines.push((depth, text.into()));
    }

    /// Splice another body in, `depth` levels deeper than this body's base.
    pub fn append(&mut self, child: Body, depth: usize) {
        for (d, text) in child.lines {
            self.lines.push((d + depth, text));
        }
    }

    pub fn render(&self, base: usize, out: &mut String) {
        for (depth, text) in &self.lines {
            for _ in 0..(base + depth) {
                out.push_str(INDENT);
            }
            out.push_str(text);
            out.push('\n');
        }
    }
}

/// One `(func ...)`: signature line, local declarations, body.
#[derive(Debug, Clone)]
pub struct Func {
    /// The opening line without its closing paren, e.g.
    /// `(func $_start (export "_start") (result i32)`.
    pub signature: String,
    /// Full local declarations, e.g. `(local $x i32)`.
    pub locals: Vec<String>,
    pub body: Body,
}

impl Func {
    fn render(&self, out: &mut String) {
        out.push_str(INDENT);
        out.push_str(&self.signature);
        out.push('\n');
        for local in &self.locals {
            out.push_str(INDENT);
            out.push_str(INDENT);
            out.push_str(local);
            out.push('\n');
        }
        self.body.render(2, out);
        out.push_str(INDENT);
        out.push_str(")\n");
    }
}

/// A whole `(module ...)`, rendered section by section.
#[derive(Debug, Clone, Default)]
pub struct Module {
    /// `(type ...)` entries — GC struct/array types once the boxed model lands.
    pub types: Vec<String>,
    pub imports: Vec<String>,
    pub globals: Vec<String>,
    pub funcs: Vec<Func>,
}

impl Module {
    pub fn render(&self) -> String {
        let mut out = String::from("(module\n");
        for section in [&self.types, &self.imports, &self.globals] {
            for entry in section {
                out.push_str(INDENT);
                out.push_str(entry);
                out.push('\n');
            }
        }
        for f in &self.funcs {
            f.render(&mut out);
        }
        out.push_str(")\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_nest_with_relative_indentation() {
        let mut inner = Body::new();
        inner.push("(call $f)");
        let mut outer = Body::new();
        outer.push("(block $b");
        outer.append(inner, 1);
        outer.push(")");
        let mut s = String::new();
        outer.render(1, &mut s);
        assert_eq!(s, "  (block $b\n    (call $f)\n  )\n");
    }

    #[test]
    fn module_renders_sections_in_order() {
        let mut body = Body::new();
        body.push("(i32.const 0)");
        let m = Module {
            types: vec!["(type $t (func))".into()],
            imports: vec!["(import \"env\" \"f\" (func $f))".into()],
            globals: vec![],
            funcs: vec![Func {
                signature: "(func $_start (export \"_start\") (result i32)".into(),
                locals: vec!["(local $x i32)".into()],
                body,
            }],
        };
        let wat = m.render();
        assert!(wat.starts_with("(module\n  (type $t (func))\n  (import"));
        assert!(wat.contains("(func $_start (export \"_start\") (result i32)\n    (local $x i32)\n    (i32.const 0)\n  )\n"));
        assert!(wat.ends_with(")\n"));
    }
}
