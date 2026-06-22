//! AST -> textual LLVM IR — Phase 0 of the native Pico 2 W backend.
//!
//! This establishes the AST -> LLVM seam (see `PICO_BACKEND.md`) the same way
//! `codegen.rs` establishes AST -> WAT: by hand-emitting the IR as text, so the
//! crate needs no LLVM build dependency. Turning this `.ll` into an actual
//! RP2350 binary (`llc` -> `lld` -> boot block -> ELF -> UF2) is a later,
//! toolchain-gated phase.
//!
//! Scope (Phase 0): the **integer** slice only — top-level integer assignments,
//! integer arithmetic, and `print(<int>)`. Everything else is a clean `Err`
//! ("the native backend doesn't handle X yet"), because the on-device value
//! model for strings/lists/dicts (an allocator + runtime) is the big later phase.
//! `print` lowers to a call to the runtime function `@p2w_print_int`, which the
//! device runtime will implement over USB-CDC (the bare-metal mirror of the
//! browser's `env.write_char`).

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// Emit a textual LLVM IR module for the supported integer subset of `stmts`,
/// or an error naming the first unsupported construct.
pub fn emit_llvm_ir(stmts: &[Stmt]) -> Result<String, String> {
    let mut e = Emitter::default();
    for s in stmts {
        e.stmt(s)?;
    }
    Ok(format!(
        "; LLVM IR — rust-p2w native (Pico) backend, Phase 0 (integer subset)\n\
         ; `print` calls into the device runtime (USB-CDC), like env.write_char.\n\
         declare void @p2w_print_int(i32)\n\
         \n\
         define i32 @main() {{\n\
         entry:\n\
         {}  ret i32 0\n\
         }}\n",
        e.body
    ))
}

#[derive(Default)]
struct Emitter {
    body: String,
    /// Next SSA temporary number.
    next: usize,
    /// Variable name -> its `alloca` pointer register (e.g. `%v_x`).
    vars: HashMap<String, String>,
}

impl Emitter {
    fn line(&mut self, s: &str) {
        self.body.push_str("  ");
        self.body.push_str(s);
        self.body.push('\n');
    }

    fn temp(&mut self) -> String {
        let t = format!("%t{}", self.next);
        self.next += 1;
        t
    }

    fn stmt(&mut self, s: &Stmt) -> Result<(), String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                s.line
            ))
        };
        match &s.kind {
            StmtKind::Assign(name, value) => {
                let v = self.expr(value)?;
                // Allocate the variable's slot on first assignment.
                let ptr = match self.vars.get(name) {
                    Some(p) => p.clone(),
                    None => {
                        let p = format!("%v_{name}");
                        self.line(&format!("{p} = alloca i32"));
                        self.vars.insert(name.clone(), p.clone());
                        p
                    }
                };
                self.line(&format!("store i32 {v}, ptr {ptr}"));
                Ok(())
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => {
                    if args.len() != 1 {
                        return nope("print() with multiple arguments");
                    }
                    let v = self.expr(&args[0])?;
                    self.line(&format!("call void @p2w_print_int(i32 {v})"));
                    Ok(())
                }
                _ => nope("this statement"),
            },
            _ => nope("this statement"),
        }
    }

    /// Emit code to evaluate an integer expression; returns its LLVM operand (an
    /// immediate like `5` or a register like `%t3`).
    fn expr(&mut self, e: &Expr) -> Result<String, String> {
        let nope = |what: &str| {
            Err(format!(
                "line {}: the native (Pico) backend doesn't handle {what} yet",
                e.line
            ))
        };
        match &e.kind {
            ExprKind::Int(n) => Ok((*n as i32).to_string()),
            ExprKind::Name(name) => {
                let ptr = self
                    .vars
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("line {}: name '{name}' is not defined", e.line))?;
                let t = self.temp();
                self.line(&format!("{t} = load i32, ptr {ptr}"));
                Ok(t)
            }
            ExprKind::Unary(UnOp::Neg, inner) => {
                let v = self.expr(inner)?;
                let t = self.temp();
                self.line(&format!("{t} = sub i32 0, {v}"));
                Ok(t)
            }
            ExprKind::Bin(op, a, b) => {
                let va = self.expr(a)?;
                let vb = self.expr(b)?;
                let opcode = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::Mul => "mul",
                    BinOp::FloorDiv => "sdiv",
                    BinOp::Mod => "srem",
                    _ => return nope("this operator"),
                };
                let t = self.temp();
                self.line(&format!("{t} = {opcode} i32 {va}, {vb}"));
                Ok(t)
            }
            _ => nope("this expression"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ir(src: &str) -> String {
        let tokens = crate::lexer::lex(src).unwrap();
        let stmts = crate::parser::parse(&tokens).unwrap();
        emit_llvm_ir(&stmts).unwrap()
    }

    #[test]
    fn emits_module_scaffold_and_print() {
        let out = ir("print(6 * 7)\n");
        assert!(out.contains("declare void @p2w_print_int(i32)"), "{out}");
        assert!(out.contains("define i32 @main()"), "{out}");
        assert!(out.contains("mul i32 6, 7"), "{out}");
        assert!(out.contains("call void @p2w_print_int(i32 %t0)"), "{out}");
        assert!(out.contains("ret i32 0"), "{out}");
    }

    #[test]
    fn assignment_loads_and_stores() {
        let out = ir("x = 2 + 3 * 4\nprint(x)\n");
        assert!(out.contains("%v_x = alloca i32"), "{out}");
        assert!(out.contains("mul i32 3, 4"), "{out}");
        assert!(out.contains("add i32 2,"), "{out}");
        assert!(out.contains("store i32"), "{out}");
        assert!(out.contains("load i32, ptr %v_x"), "{out}");
    }

    #[test]
    fn arithmetic_opcodes() {
        assert!(ir("print(10 - 4)\n").contains("sub i32 10, 4"));
        assert!(ir("print(9 // 2)\n").contains("sdiv i32 9, 2"));
        assert!(ir("print(9 % 2)\n").contains("srem i32 9, 2"));
        assert!(ir("print(-5)\n").contains("sub i32 0, 5"));
    }

    #[test]
    fn unsupported_constructs_are_clean_errors() {
        let err = emit_llvm_ir(&parse("s = \"hi\"\nprint(s)\n")).unwrap_err();
        assert!(err.contains("native"), "{err}");
        let err = emit_llvm_ir(&parse("for i in range(3):\n    print(i)\n")).unwrap_err();
        assert!(err.contains("native"), "{err}");
    }

    fn parse(src: &str) -> Vec<Stmt> {
        crate::parser::parse(&crate::lexer::lex(src).unwrap()).unwrap()
    }
}
