//! Python source -> Blockly workspace XML, for the IDE's text->blocks
//! direction (the reverse of Blockly's blocks->Python generator).
//!
//! Only the constructs that have a standard Blockly block are representable:
//! assignment, `if`/`elif`/`else`, `while`, counted `for`, `break`/`continue`,
//! single-argument `print`, numbers/strings/booleans/variables, arithmetic,
//! comparisons, and `and`/`or`/`not`. Anything else is a clean `Err` so the IDE
//! can keep the text as the source of truth rather than dropping code.

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};

/// Convert Python source to Blockly workspace XML, or an error naming the first
/// construct that has no block yet.
pub fn to_blockly_xml(source: &str) -> Result<String, String> {
    let tokens = crate::lexer::lex(source).map_err(|e| e.to_string())?;
    let stmts = crate::parser::parse(&tokens).map_err(|e| e.to_string())?;
    let mut b = Builder::default();
    b.collect_vars_stmts(&stmts);
    let body = b.chain(&stmts)?;
    Ok(format!(
        "<xml xmlns=\"https://developers.google.com/blockly/xml\">{}{}</xml>",
        b.variables_xml(),
        body
    ))
}

#[derive(Default)]
struct Builder {
    /// Variable name -> stable Blockly id, in first-seen order.
    vars: Vec<(String, String)>,
}

impl Builder {
    fn var_id(&mut self, name: &str) -> String {
        if let Some((_, id)) = self.vars.iter().find(|(n, _)| n == name) {
            return id.clone();
        }
        let id = format!("var_{}", self.vars.len());
        self.vars.push((name.to_string(), id.clone()));
        id
    }

    fn variables_xml(&self) -> String {
        if self.vars.is_empty() {
            return String::new();
        }
        let mut s = String::from("<variables>");
        for (name, id) in &self.vars {
            s.push_str(&format!(
                "<variable id=\"{id}\">{}</variable>",
                escape(name)
            ));
        }
        s.push_str("</variables>");
        s
    }

    /// Pre-pass: register every variable name so `<variables>` is complete
    /// before any `variables_get`/`variables_set` references it.
    fn collect_vars_stmts(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match &s.kind {
                StmtKind::Assign(name, e) => {
                    self.var_id(name);
                    self.collect_vars_expr(e);
                }
                StmtKind::For {
                    var,
                    start,
                    end,
                    step,
                    body,
                } => {
                    self.var_id(var);
                    self.collect_vars_expr(start);
                    self.collect_vars_expr(end);
                    self.collect_vars_expr(step);
                    self.collect_vars_stmts(body);
                }
                StmtKind::While { cond, body } => {
                    self.collect_vars_expr(cond);
                    self.collect_vars_stmts(body);
                }
                StmtKind::If {
                    cond,
                    body,
                    elifs,
                    else_body,
                } => {
                    self.collect_vars_expr(cond);
                    self.collect_vars_stmts(body);
                    for (c, b) in elifs {
                        self.collect_vars_expr(c);
                        self.collect_vars_stmts(b);
                    }
                    if let Some(b) = else_body {
                        self.collect_vars_stmts(b);
                    }
                }
                StmtKind::Expr(e) => self.collect_vars_expr(e),
                _ => {}
            }
        }
    }

    fn collect_vars_expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Name(n) => {
                self.var_id(n);
            }
            ExprKind::Unary(_, inner) => self.collect_vars_expr(inner),
            ExprKind::Bin(_, a, b) => {
                self.collect_vars_expr(a);
                self.collect_vars_expr(b);
            }
            ExprKind::Call(_, args) => {
                for a in args {
                    self.collect_vars_expr(a);
                }
            }
            _ => {}
        }
    }

    /// A vertical chain of statement blocks: the first block, with the rest
    /// nested in its `<next>`.
    fn chain(&mut self, stmts: &[Stmt]) -> Result<String, String> {
        let Some((first, rest)) = stmts.split_first() else {
            return Ok(String::new());
        };
        let rest_xml = self.chain(rest)?;
        let next = if rest_xml.is_empty() {
            String::new()
        } else {
            format!("<next>{rest_xml}</next>")
        };
        self.stmt_block(first, &next)
    }

    fn stmt_block(&mut self, s: &Stmt, next: &str) -> Result<String, String> {
        let unsupported = |what: &str| {
            Err(format!(
                "line {}: {what} has no block yet (edit it in the text pane)",
                s.line
            ))
        };
        match &s.kind {
            StmtKind::Assign(name, value) => {
                let id = self.var_id(name);
                let v = self.value_block(value)?;
                Ok(format!(
                    "<block type=\"variables_set\"><field name=\"VAR\" id=\"{id}\">{}</field><value name=\"VALUE\">{v}</value>{next}</block>",
                    escape(name)
                ))
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => {
                    if args.len() != 1 {
                        return unsupported("print() with multiple arguments");
                    }
                    let v = self.value_block(&args[0])?;
                    Ok(format!(
                        "<block type=\"text_print\"><value name=\"TEXT\">{v}</value>{next}</block>"
                    ))
                }
                _ => unsupported("this statement"),
            },
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                let mut inner = format!(
                    "<value name=\"IF0\">{}</value><statement name=\"DO0\">{}</statement>",
                    self.value_block(cond)?,
                    self.chain(body)?
                );
                for (i, (c, b)) in elifs.iter().enumerate() {
                    let n = i + 1;
                    inner.push_str(&format!(
                        "<value name=\"IF{n}\">{}</value><statement name=\"DO{n}\">{}</statement>",
                        self.value_block(c)?,
                        self.chain(b)?
                    ));
                }
                if let Some(b) = else_body {
                    inner.push_str(&format!(
                        "<statement name=\"ELSE\">{}</statement>",
                        self.chain(b)?
                    ));
                }
                let mutation = format!(
                    "<mutation elseif=\"{}\" else=\"{}\"></mutation>",
                    elifs.len(),
                    else_body.is_some() as u8
                );
                Ok(format!(
                    "<block type=\"controls_if\">{mutation}{inner}{next}</block>"
                ))
            }
            StmtKind::While { cond, body } => Ok(format!(
                "<block type=\"controls_whileUntil\"><field name=\"MODE\">WHILE</field><value name=\"BOOL\">{}</value><statement name=\"DO\">{}</statement>{next}</block>",
                self.value_block(cond)?,
                self.chain(body)?
            )),
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let id = self.var_id(var);
                // Blockly's `controls_for` TO bound is inclusive, but Python's
                // range() end is exclusive — represent it as `end - 1`.
                let to = self.inclusive_to(end)?;
                Ok(format!(
                    "<block type=\"controls_for\"><field name=\"VAR\" id=\"{id}\">{}</field><value name=\"FROM\">{}</value><value name=\"TO\">{to}</value><value name=\"BY\">{}</value><statement name=\"DO\">{}</statement>{next}</block>",
                    escape(var),
                    self.value_block(start)?,
                    self.value_block(step)?,
                    self.chain(body)?
                ))
            }
            StmtKind::Break => Ok(format!(
                "<block type=\"controls_flow_statements\"><field name=\"FLOW\">BREAK</field>{next}</block>"
            )),
            StmtKind::Continue => Ok(format!(
                "<block type=\"controls_flow_statements\"><field name=\"FLOW\">CONTINUE</field>{next}</block>"
            )),
            StmtKind::ForEach { .. } => unsupported("`for ... in <list>`"),
            StmtKind::Def { .. } => unsupported("function definitions"),
            StmtKind::ClassDef { .. } => unsupported("classes"),
            StmtKind::Return(_) => unsupported("`return`"),
            StmtKind::SetIndex { .. } => unsupported("item assignment"),
            StmtKind::SetAttr { .. } => unsupported("attribute assignment"),
            StmtKind::UnpackAssign { .. } => unsupported("tuple unpacking"),
            StmtKind::Import(_) => unsupported("`import`"),
        }
    }

    /// `end - 1` as a Blockly value (folded when `end` is a literal int).
    fn inclusive_to(&mut self, end: &Expr) -> Result<String, String> {
        if let ExprKind::Int(n) = end.kind {
            return Ok(number(n as f64 - 1.0));
        }
        let e = self.value_block(end)?;
        Ok(format!(
            "<block type=\"math_arithmetic\"><field name=\"OP\">MINUS</field><value name=\"A\">{e}</value><value name=\"B\">{}</value></block>",
            number(1.0)
        ))
    }

    fn value_block(&mut self, e: &Expr) -> Result<String, String> {
        let unsupported = |what: &str| {
            Err(format!(
                "line {}: {what} has no block yet (edit it in the text pane)",
                e.line
            ))
        };
        match &e.kind {
            ExprKind::Int(n) => Ok(number(*n as f64)),
            ExprKind::Float(f) => Ok(number(*f)),
            ExprKind::Bool(v) => Ok(format!(
                "<block type=\"logic_boolean\"><field name=\"BOOL\">{}</field></block>",
                if *v { "TRUE" } else { "FALSE" }
            )),
            ExprKind::Str(s) => Ok(format!(
                "<block type=\"text\"><field name=\"TEXT\">{}</field></block>",
                escape(s)
            )),
            ExprKind::Name(n) => {
                let id = self.var_id(n);
                Ok(format!(
                    "<block type=\"variables_get\"><field name=\"VAR\" id=\"{id}\">{}</field></block>",
                    escape(n)
                ))
            }
            ExprKind::Unary(UnOp::Not, inner) => Ok(format!(
                "<block type=\"logic_negate\"><value name=\"BOOL\">{}</value></block>",
                self.value_block(inner)?
            )),
            ExprKind::Unary(UnOp::Neg, inner) => {
                if let ExprKind::Int(n) = inner.kind {
                    return Ok(number(-(n as f64)));
                }
                if let ExprKind::Float(f) = inner.kind {
                    return Ok(number(-f));
                }
                // -x as 0 - x.
                Ok(format!(
                    "<block type=\"math_arithmetic\"><field name=\"OP\">MINUS</field><value name=\"A\">{}</value><value name=\"B\">{}</value></block>",
                    number(0.0),
                    self.value_block(inner)?
                ))
            }
            ExprKind::Bin(op, a, b) => self.bin_block(*op, a, b),
            ExprKind::Call(..) => unsupported("a function call"),
            ExprKind::MethodCall(..) => unsupported("a method call"),
            ExprKind::List(_) => unsupported("a list"),
            ExprKind::Tuple(_) => unsupported("a tuple"),
            ExprKind::Dict(_) => unsupported("a dict"),
            ExprKind::Index(..) | ExprKind::Slice { .. } => unsupported("indexing/slicing"),
            ExprKind::Attr(..) => unsupported("an attribute"),
            ExprKind::ListComp { .. } | ExprKind::DictComp { .. } => unsupported("a comprehension"),
            ExprKind::NoneLit => unsupported("None"),
            ExprKind::Kwarg(..) => unsupported("a keyword argument"),
        }
    }

    fn bin_block(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<String, String> {
        let av = self.value_block(a)?;
        let bv = self.value_block(b)?;
        // (block_type, op_field) for the standard math/logic blocks.
        let arith = |op: &str| {
            format!(
                "<block type=\"math_arithmetic\"><field name=\"OP\">{op}</field><value name=\"A\">{av}</value><value name=\"B\">{bv}</value></block>"
            )
        };
        let compare = |op: &str| {
            format!(
                "<block type=\"logic_compare\"><field name=\"OP\">{op}</field><value name=\"A\">{av}</value><value name=\"B\">{bv}</value></block>"
            )
        };
        Ok(match op {
            BinOp::Add => arith("ADD"),
            BinOp::Sub => arith("MINUS"),
            BinOp::Mul => arith("MULTIPLY"),
            BinOp::Div | BinOp::FloorDiv => arith("DIVIDE"),
            BinOp::Pow => arith("POWER"),
            BinOp::Mod => format!(
                "<block type=\"math_modulo\"><value name=\"DIVIDEND\">{av}</value><value name=\"DIVISOR\">{bv}</value></block>"
            ),
            BinOp::Eq => compare("EQ"),
            BinOp::Ne => compare("NEQ"),
            BinOp::Lt => compare("LT"),
            BinOp::Le => compare("LTE"),
            BinOp::Gt => compare("GT"),
            BinOp::Ge => compare("GTE"),
            BinOp::And => format!(
                "<block type=\"logic_operation\"><field name=\"OP\">AND</field><value name=\"A\">{av}</value><value name=\"B\">{bv}</value></block>"
            ),
            BinOp::Or => format!(
                "<block type=\"logic_operation\"><field name=\"OP\">OR</field><value name=\"A\">{av}</value><value name=\"B\">{bv}</value></block>"
            ),
            BinOp::In | BinOp::NotIn | BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => {
                return Err(format!(
                    "line {}: this operator has no block yet (edit it in the text pane)",
                    a.line
                ))
            }
        })
    }
}

/// A `math_number` block. Whole values print without a trailing `.0`.
fn number(n: f64) -> String {
    let field = if n.fract() == 0.0 && n.is_finite() {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    };
    format!("<block type=\"math_number\"><field name=\"NUM\">{field}</field></block>")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::to_blockly_xml;

    #[test]
    fn assignment_and_print() {
        let xml = to_blockly_xml("x = 5\nprint(x)").unwrap();
        assert!(xml.contains("<variable id=\"var_0\">x</variable>"));
        assert!(xml.contains("type=\"variables_set\""));
        assert!(xml.contains("<field name=\"NUM\">5</field>"));
        assert!(xml.contains("type=\"text_print\""));
        // print(x) is chained after the assignment.
        assert!(xml.contains("<next>"));
    }

    #[test]
    fn arithmetic_and_comparison() {
        let xml = to_blockly_xml("y = 2 + 3 * 4").unwrap();
        assert!(xml.contains("<field name=\"OP\">ADD</field>"));
        assert!(xml.contains("<field name=\"OP\">MULTIPLY</field>"));
        let xml = to_blockly_xml("z = 1 < 2").unwrap();
        assert!(xml.contains("type=\"logic_compare\""));
        assert!(xml.contains("<field name=\"OP\">LT</field>"));
    }

    #[test]
    fn if_while_for() {
        let xml = to_blockly_xml(
            "if x < 3:\n    print(x)\nelif x < 5:\n    print(1)\nelse:\n    print(2)",
        )
        .unwrap();
        assert!(xml.contains("type=\"controls_if\""));
        assert!(xml.contains("<mutation elseif=\"1\" else=\"1\">"));

        let xml = to_blockly_xml("while x < 10:\n    x = x + 1").unwrap();
        assert!(xml.contains("type=\"controls_whileUntil\""));

        // range(1, 5) -> controls_for with inclusive TO = 4.
        let xml = to_blockly_xml("for i in range(1, 5):\n    print(i)").unwrap();
        assert!(xml.contains("type=\"controls_for\""));
        assert!(xml.contains("<field name=\"NUM\">4</field>"));
    }

    #[test]
    fn booleans_and_logic() {
        let xml = to_blockly_xml("ok = True and not False").unwrap();
        assert!(xml.contains("type=\"logic_boolean\""));
        assert!(xml.contains("type=\"logic_operation\""));
        assert!(xml.contains("type=\"logic_negate\""));
    }

    #[test]
    fn strings_are_escaped() {
        let xml = to_blockly_xml("print(\"a < b & c\")").unwrap();
        assert!(xml.contains("a &lt; b &amp; c"));
    }

    #[test]
    fn unsupported_constructs_error_gracefully() {
        let err = to_blockly_xml("xs = [1, 2, 3]").unwrap_err();
        assert!(err.contains("list"), "{err}");
        let err = to_blockly_xml("def f():\n    return 1").unwrap_err();
        assert!(err.contains("function"), "{err}");
    }

    #[test]
    fn break_and_continue() {
        let xml = to_blockly_xml("while True:\n    if x:\n        break\n    continue").unwrap();
        assert!(xml.contains("<field name=\"FLOW\">BREAK</field>"));
        assert!(xml.contains("<field name=\"FLOW\">CONTINUE</field>"));
    }
}
