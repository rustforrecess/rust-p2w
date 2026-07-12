//! Python source -> Blockly workspace JSON, for the IDE's text->blocks
//! direction (the reverse of Blockly's blocks->Python generator).
//!
//! Emits Blockly's modern JSON serialization format (the shape consumed by
//! `Blockly.serialization.workspaces.load`); the older XML serialization is
//! "iceboxed" upstream (supported but frozen). A workspace is one tree:
//!
//! ```json
//! { "blocks": { "languageVersion": 0, "blocks": [ <top block> ] },
//!   "variables": [ { "name": "x", "id": "var_0" } ] }
//! ```
//!
//! Most constructs map to a standard Blockly block: assignment,
//! `if`/`elif`/`else`, `while`, counted `for`, for-each, `break`/`continue`,
//! single-argument `print`, list literals, numbers/strings/booleans/variables,
//! arithmetic, comparisons, and `and`/`or`/`not`. Functions use our own
//! Python-shaped custom blocks (`python_def`, `python_return`,
//! `python_call_statement`, `python_call_value`) — see `blockly-init.js`.
//! Anything else is a clean `Err` so the IDE can keep the text as the source of
//! truth rather than dropping code.
//!
//! The JSON is assembled as plain strings, bottom-up: the small free functions
//! [`block`], [`field`], [`input`], [`var_ref`] and [`number`] are the
//! constructors, and each `stmt_block`/`value_block` arm composes them into one
//! block object. Hand-built on purpose, so the crate keeps zero runtime
//! dependencies (no serde).

use crate::ast::{BinOp, Expr, ExprKind, Stmt, StmtKind, UnOp};
use crate::error::CompileError;

/// The result of a forgiving text->blocks conversion: a Blockly workspace JSON
/// document (always valid — possibly an empty workspace) plus any diagnostics.
///
/// Unlike [`to_blockly_json`], this never gives up on the first problem. The
/// parser recovers past syntax errors, and each top-level statement is rendered
/// independently, so one mistake (or one not-yet-supported construct) doesn't
/// blank the canvas — the rest still becomes blocks, and the issues are
/// reported for the editor to show.
pub struct BlocksOutcome {
    pub json: String,
    /// All diagnostics, for the editor to show as gentle hints: real syntax
    /// errors AND "this valid code just has no block yet" notes.
    pub errors: Vec<CompileError>,
    /// Lines of *syntax* errors only (from the parser) — safe to highlight in
    /// the editor as mistakes. Deliberately excludes the not-yet-representable
    /// notes, since that code is valid Python and shouldn't be flagged as wrong.
    pub error_lines: Vec<usize>,
    /// Byte spans `[start, end)` of those same errors that carry one — for
    /// underlining the exact offending text (a squiggle) instead of the whole
    /// line. A subset of the errors behind `error_lines` (some are line-only).
    pub error_spans: Vec<(usize, usize)>,
}

/// Forgiving Python -> Blockly conversion for the live editor. See
/// [`BlocksOutcome`].
pub fn to_blocks(source: &str) -> BlocksOutcome {
    let tokens = match crate::lexer::lex(source) {
        Ok(t) => t,
        // Lexing is still all-or-nothing (e.g. an unterminated string); report
        // it and leave the canvas empty.
        Err(e) => {
            return BlocksOutcome {
                json: "{\"blocks\":{\"languageVersion\":0,\"blocks\":[]}}".to_string(),
                error_lines: e.line.into_iter().collect(),
                error_spans: e.span.into_iter().collect(),
                errors: vec![e],
            };
        }
    };
    let (stmts, parse_errors) = crate::parser::parse_recovering(&tokens);
    to_blocks_parsed(&stmts, parse_errors)
}

/// The post-parse half of [`to_blocks`], callable with an already-parsed
/// program so [`crate::analyze`] can share ONE lex+parse between the blocks
/// conversion and the lint set (they run together on every editor keystroke).
pub(crate) fn to_blocks_parsed(
    stmts: &[crate::ast::Stmt],
    parse_errors: Vec<crate::CompileError>,
) -> BlocksOutcome {
    // Highlightable mistakes: genuine syntax errors, plus typo'd function calls
    // (`pint` -> `print`) — both are real errors. The build notes below are for
    // valid-but-unrepresentable code and must NOT flag the editor.
    let mut error_lines: Vec<usize> = parse_errors.iter().filter_map(|e| e.line).collect();
    let mut error_spans: Vec<(usize, usize)> = parse_errors.iter().filter_map(|e| e.span).collect();
    let mut errors = parse_errors;

    let typos = crate::lint::typo_diagnostics(&stmts);
    let typo_lines: Vec<usize> = typos.iter().filter_map(|e| e.line).collect();
    error_lines.extend(typo_lines.iter().copied());
    error_spans.extend(typos.iter().filter_map(|e| e.span));
    errors.extend(typos);

    let mut b = Builder {
        tolerant: true,
        ..Default::default()
    };
    let tops = b.build_program(&stmts);
    let json = b.document(&tops);
    for note in b.notes {
        // A typo'd call already has a clearer "did you mean" message on this
        // line; don't also say the vaguer "no block yet".
        if note.line.is_some_and(|l| typo_lines.contains(&l)) {
            continue;
        }
        errors.push(note);
    }

    error_lines.sort_unstable();
    error_lines.dedup();
    error_spans.sort_unstable();
    error_spans.dedup();
    errors.sort_by_key(|e| e.line.unwrap_or(usize::MAX));

    BlocksOutcome {
        json,
        errors,
        error_lines,
        error_spans,
    }
}

/// Convert Python source to a Blockly workspace JSON document, or an error
/// naming the first construct that has no block yet.
pub fn to_blockly_json(source: &str) -> Result<String, String> {
    let tokens = crate::lexer::lex(source).map_err(|e| e.to_string())?;
    let stmts = crate::parser::parse(&tokens).map_err(|e| e.to_string())?;
    let mut b = Builder::default();
    // `chain` registers every variable it references via `var_id`, and
    // `variables_json` runs afterwards, so the variable list is complete with no
    // separate pre-pass.
    let body = b.chain(&stmts)?;
    // Give the single top-level block an (x, y) so Blockly doesn't drop it at
    // the origin under the toolbox. `body` is one complete block object —
    // `{"type":...}` — so splice the coordinates in right after its opening
    // brace (`&body[1..]` is everything past the leading `{`).
    let top = if body.is_empty() {
        String::new()
    } else {
        format!("{{\"x\":20,\"y\":20,{}", &body[1..])
    };
    let mut out = format!("{{\"blocks\":{{\"languageVersion\":0,\"blocks\":[{top}]}}");
    out.push_str(&b.variables_json());
    out.push('}');
    Ok(out)
}

#[derive(Default)]
struct Builder {
    /// Variable name -> stable Blockly id, in first-seen order.
    vars: Vec<(String, String)>,
    /// When true (the `to_blocks` path), rendering a statement that can't be
    /// represented records a note and is skipped instead of aborting — so one
    /// bad line inside a loop/if body doesn't drop the whole compound. When
    /// false (the strict `to_blockly_json` path), the first failure propagates.
    tolerant: bool,
    /// Notes gathered while tolerant: statements that couldn't be represented.
    notes: Vec<CompileError>,
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

    /// The `,"variables":[...]` tail of the document (empty when no variables).
    fn variables_json(&self) -> String {
        if self.vars.is_empty() {
            return String::new();
        }
        let entries: Vec<String> = self
            .vars
            .iter()
            .map(|(name, id)| format!("{{\"name\":{},\"id\":{}}}", jstr(name), jstr(id)))
            .collect();
        format!(",\"variables\":[{}]", entries.join(","))
    }

    /// Render top-level statements forgivingly: each is built on its own, and a
    /// statement that can't be represented (a not-yet-supported construct) is
    /// skipped — noted in `self.notes` — instead of aborting. Consecutive
    /// renderable statements are linked into one connected stack via `next`; a
    /// skipped one breaks the stack, so a fresh stack starts after it. Returns
    /// the top-level (unpositioned) block stacks.
    fn build_program(&mut self, stmts: &[Stmt]) -> Vec<String> {
        let mut tops = Vec::new();
        let mut run: Vec<String> = Vec::new();
        let mut i = 0;
        while i < stmts.len() {
            // Fold the event-handler pattern (def + registration) into one hat.
            if let Some((kind, name, body)) = match_event_hat(&stmts[i..]) {
                match self.event_hat_block(&kind, name, body, "", stmts[i].line) {
                    Ok(block) => run.push(block),
                    Err(msg) => {
                        flush_run(&mut run, &mut tops);
                        self.notes.push(note_from(&msg));
                    }
                }
                i += 2;
                continue;
            }
            match self.stmt_block(&stmts[i], "") {
                Ok(block) => run.push(block),
                Err(msg) => {
                    flush_run(&mut run, &mut tops);
                    self.notes.push(note_from(&msg));
                }
            }
            i += 1;
        }
        flush_run(&mut run, &mut tops);
        tops
    }

    /// Wrap top-level block stacks into a workspace document, giving each stack
    /// its own (x, y) so separate stacks (after an error break) don't overlap.
    fn document(&self, tops: &[String]) -> String {
        let positioned: Vec<String> = tops
            .iter()
            .enumerate()
            .map(|(i, t)| format!("{{\"x\":20,\"y\":{},{}", 20 + i * 60, &t[1..]))
            .collect();
        let mut out = format!(
            "{{\"blocks\":{{\"languageVersion\":0,\"blocks\":[{}]}}",
            positioned.join(",")
        );
        out.push_str(&self.variables_json());
        out.push('}');
        out
    }

    /// A vertical chain of statement blocks: the first block, with the rest
    /// nested in its `next`. Returns "" for an empty statement list.
    ///
    /// In tolerant mode (used for a compound's body), a statement that can't be
    /// represented is noted and skipped, and the rest stay linked — so one bad
    /// line inside a loop/if body doesn't discard the whole body. In strict mode
    /// the first failure propagates.
    ///
    /// Before rendering a statement normally, we peek for the event-handler
    /// pattern (a zero-arg `def` immediately followed by its registration call)
    /// and fold that pair into a single "event hat" block, so blocks and text
    /// stay in sync for the interactive-web event seam.
    fn chain(&mut self, stmts: &[Stmt]) -> Result<String, String> {
        // `pass` is a pure no-op: drop it so an all-`pass` body renders as an
        // EMPTY block body. The JS generator re-emits `pass` for an empty body,
        // so `def f(): pass` round-trips. (A `pass` with siblings is redundant
        // and safely dropped too.) Filtering here covers every nesting level,
        // since each nested body is rendered through `chain`.
        if stmts.iter().any(|s| matches!(s.kind, StmtKind::Pass)) {
            let kept: Vec<Stmt> = stmts
                .iter()
                .filter(|s| !matches!(s.kind, StmtKind::Pass))
                .cloned()
                .collect();
            return self.chain(&kept);
        }
        if self.tolerant {
            let mut rendered: Vec<String> = Vec::new();
            let mut i = 0;
            while i < stmts.len() {
                if let Some((kind, name, body)) = match_event_hat(&stmts[i..]) {
                    match self.event_hat_block(&kind, name, body, "", stmts[i].line) {
                        Ok(b) => rendered.push(b),
                        Err(msg) => self.notes.push(note_from(&msg)),
                    }
                    i += 2;
                    continue;
                }
                match self.stmt_block(&stmts[i], "") {
                    Ok(b) => rendered.push(b),
                    Err(msg) => self.notes.push(note_from(&msg)),
                }
                i += 1;
            }
            return Ok(stitch(rendered));
        }
        let Some((first, _rest)) = stmts.split_first() else {
            return Ok(String::new());
        };
        if let Some((kind, name, body)) = match_event_hat(stmts) {
            // A hat consumes exactly the def + its registration call.
            let rest_json = self.chain(&stmts[2..])?;
            return self.event_hat_block(&kind, name, body, &rest_json, first.line);
        }
        let rest_json = self.chain(&stmts[1..])?;
        self.stmt_block(first, &rest_json)
    }

    /// Build an event-hat block (`python_when_click` / `python_when_key` /
    /// `python_every`) from a matched handler def: the handler name and the
    /// event-specific field, with the def body in the `DO` stack. Mirrors the JS
    /// blocks in `assets/blockly-init.js`.
    fn event_hat_block(
        &mut self,
        kind: &HatKind,
        name: &str,
        body: &[Stmt],
        next: &str,
        line: usize,
    ) -> Result<String, String> {
        let stack = self.chain(body)?;
        let inputs = if stack.is_empty() {
            String::new()
        } else {
            input("DO", &stack)
        };
        let (ty, event_field) = match kind {
            HatKind::Click(sel) => ("python_when_click", field("SELECTOR", &jstr(sel))),
            HatKind::Key(key) => ("python_when_key", field("KEY", &jstr(key))),
            HatKind::Every(ms) => ("python_every", field("MS", &jstr(ms))),
        };
        let fields = format!("{},{}", event_field, field("NAME", &jstr(name)));
        Ok(with_data(&block(ty, &fields, &inputs, "", next), line))
    }

    /// Build a statement's block and tag it with its 1-based source line in the
    /// Blockly `data` field, so the step debugger can highlight the block for the
    /// line it's paused on. `data` round-trips through serialization and is
    /// ignored by the blocks->Python generator, so it's a pure annotation.
    fn stmt_block(&mut self, s: &Stmt, next: &str) -> Result<String, String> {
        let json = self.stmt_block_raw(s, next)?;
        Ok(with_data(&json, s.line))
    }

    /// An input carrying `e` — or NOTHING (empty socket) when `e` is the `None`
    /// literal. This is the round-trip twin of the generator's rule "an empty
    /// socket reads back as None": a half-built block writes `f(None, …)` into
    /// the canonical text, and the debounced reload must render that as the
    /// same block with the socket still empty — NOT drop the statement, which
    /// yanked in-progress blocks off the canvas mid-edit.
    fn slot_input(&mut self, name: &str, e: &Expr) -> Result<String, String> {
        if matches!(e.kind, ExprKind::NoneLit) {
            return Ok(String::new());
        }
        Ok(input(name, &self.value_block(e)?))
    }

    fn stmt_block_raw(&mut self, s: &Stmt, next: &str) -> Result<String, String> {
        let unsupported = |what: &str| {
            Err(format!(
                "line {}: {what} has no block yet (edit it in the text pane)",
                s.line
            ))
        };
        match &s.kind {
            StmtKind::Assign(name, value) | StmtKind::AnnAssign { name, value, .. } => {
                let id = self.var_id(name);
                let v = self.slot_input("VALUE", value)?;
                Ok(block(
                    "variables_set",
                    &field("VAR", &var_ref(&id)),
                    &v,
                    "",
                    next,
                ))
            }
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Call(name, args) if name == "print" => {
                    if args.len() != 1 {
                        return unsupported("print() with multiple arguments");
                    }
                    let v = self.slot_input("TEXT", &args[0])?;
                    Ok(block("text_print", "", &v, "", next))
                }
                // Any other named call as a statement (a void call like
                // `greet("Bo")`) becomes our generic Python-shaped call block.
                ExprKind::Call(name, args) => self.call_block(name, args, true, next),
                // A method call as a statement (`xs.append(5)`).
                ExprKind::MethodCall(obj, method, args) => {
                    self.method_block(obj, method, args, true, next)
                }
                _ => unsupported("this statement"),
            },
            StmtKind::If {
                cond,
                body,
                elifs,
                else_body,
            } => {
                let mut ins = vec![input("IF0", &self.value_block(cond)?)];
                let do0 = self.chain(body)?;
                if !do0.is_empty() {
                    ins.push(input("DO0", &do0));
                }
                for (i, (c, b)) in elifs.iter().enumerate() {
                    let n = i + 1;
                    ins.push(input(&format!("IF{n}"), &self.value_block(c)?));
                    let d = self.chain(b)?;
                    if !d.is_empty() {
                        ins.push(input(&format!("DO{n}"), &d));
                    }
                }
                if let Some(b) = else_body {
                    let e = self.chain(b)?;
                    if !e.is_empty() {
                        ins.push(input("ELSE", &e));
                    }
                }
                // controls_if stores its extra inputs in extraState; the default
                // (no elifs, no else) omits it entirely.
                let extra = if !elifs.is_empty() || else_body.is_some() {
                    format!(
                        "{{\"elseIfCount\":{},\"hasElse\":{}}}",
                        elifs.len(),
                        else_body.is_some()
                    )
                } else {
                    String::new()
                };
                Ok(block("controls_if", "", &ins.join(","), &extra, next))
            }
            StmtKind::While { cond, body } => {
                let mut ins = vec![input("BOOL", &self.value_block(cond)?)];
                let d = self.chain(body)?;
                if !d.is_empty() {
                    ins.push(input("DO", &d));
                }
                Ok(block(
                    "controls_whileUntil",
                    &field("MODE", &jstr("WHILE")),
                    &ins.join(","),
                    "",
                    next,
                ))
            }
            StmtKind::For {
                var,
                start,
                end,
                step,
                body,
            } => {
                let id = self.var_id(var);
                // The range-native loop: FROM/TO are `range(start, stop)`
                // verbatim — stop stays exclusive, no ±1 fudge — so the blocks
                // and the code read as the same thing. (Blockly's stock inclusive
                // `controls_for` produced a phantom `end - 1` the student never
                // wrote.) A step of exactly 1 is Python's default, so we HIDE the
                // step socket entirely: the block reads `range(start, stop)`,
                // mirroring how the code is normally written. Any other step
                // shows the socket, with extraState driving the block's shape.
                let has_step = !step_is_one(step);
                let mut ins = vec![
                    input("FROM", &self.value_block(start)?),
                    input("TO", &self.value_block(end)?),
                ];
                if has_step {
                    ins.push(input("BY", &self.value_block(step)?));
                }
                let d = self.chain(body)?;
                if !d.is_empty() {
                    ins.push(input("DO", &d));
                }
                let extra = if has_step { "{\"hasStep\":true}" } else { "" };
                Ok(block(
                    "python_for_range",
                    &field("VAR", &var_ref(&id)),
                    &ins.join(","),
                    extra,
                    next,
                ))
            }
            StmtKind::Break => Ok(block(
                "controls_flow_statements",
                &field("FLOW", &jstr("BREAK")),
                "",
                "",
                next,
            )),
            StmtKind::Continue => Ok(block(
                "controls_flow_statements",
                &field("FLOW", &jstr("CONTINUE")),
                "",
                "",
                next,
            )),
            // `pass` is filtered out by `chain` before it reaches here (it
            // renders as an empty body); this arm is just for exhaustiveness.
            StmtKind::Pass => Ok(String::new()),
            StmtKind::ForEach {
                var,
                iterable,
                body,
            } => {
                let id = self.var_id(var);
                let mut ins = vec![input("LIST", &self.value_block(iterable)?)];
                let d = self.chain(body)?;
                if !d.is_empty() {
                    ins.push(input("DO", &d));
                }
                Ok(block(
                    "controls_forEach",
                    &field("VAR", &var_ref(&id)),
                    &ins.join(","),
                    "",
                    next,
                ))
            }
            StmtKind::Def {
                name,
                params,
                param_types,
                defaults,
                return_type,
                body,
            } => {
                // Reconstruct the signature text, folding any `: T` annotations
                // and `= default` values inline (the def block carries params as
                // free text, so defaults ride along and round-trip through the
                // generator). Defaults align to the trailing params. A type or
                // default shape we can't render keeps the whole def in text rather
                // than emit a broken signature.
                let first_default = params.len().saturating_sub(defaults.len());
                let mut parts = Vec::with_capacity(params.len());
                for (i, (p, t)) in params.iter().zip(param_types.iter()).enumerate() {
                    let mut part = match t {
                        None => p.clone(),
                        Some(ty) => {
                            let Some(src) = type_to_source(ty) else {
                                return unsupported("this type annotation");
                            };
                            format!("{p}: {src}")
                        }
                    };
                    if i >= first_default {
                        let Some(dsrc) = value_to_source(&defaults[i - first_default]) else {
                            return unsupported("this default value");
                        };
                        // PEP 8: spaces around `=` only when the param is annotated.
                        if t.is_some() {
                            part.push_str(&format!(" = {dsrc}"));
                        } else {
                            part.push_str(&format!("={dsrc}"));
                        }
                    }
                    parts.push(part);
                }
                let params_str = parts.join(", ");
                // The return annotation rides in extraState so the block rebuilds
                // its `-> T` row on load.
                let extra = match return_type {
                    Some(ty) => {
                        let Some(src) = type_to_source(ty) else {
                            return unsupported("this return type");
                        };
                        format!("{{\"returns\":{}}}", jstr(&src))
                    }
                    None => String::new(),
                };
                let fields = format!(
                    "{},{}",
                    field("NAME", &jstr(name)),
                    field("PARAMS", &jstr(&params_str))
                );
                let stack = self.chain(body)?;
                let inputs = if stack.is_empty() {
                    String::new()
                } else {
                    input("STACK", &stack)
                };
                Ok(block("python_def", &fields, &inputs, &extra, next))
            }
            StmtKind::ClassDef { .. } => unsupported("classes"),
            StmtKind::Return(value) => {
                let inputs = match value {
                    Some(e) => input("VALUE", &self.value_block(e)?),
                    None => String::new(),
                };
                Ok(block("python_return", "", &inputs, "", next))
            }
            // Subscript assignment `target[index] = value` (lists, dicts).
            StmtKind::SetIndex {
                target,
                index,
                value,
            } => {
                let t = self.value_block(target)?;
                let i = self.value_block(index)?;
                let v = self.value_block(value)?;
                Ok(block(
                    "python_set_index",
                    "",
                    &format!(
                        "{},{},{}",
                        input("TARGET", &t),
                        input("INDEX", &i),
                        input("VALUE", &v)
                    ),
                    "",
                    next,
                ))
            }
            StmtKind::SetAttr { .. } => unsupported("attribute assignment"),
            StmtKind::UnpackAssign { .. } => unsupported("tuple unpacking"),
            StmtKind::Import(_) => unsupported("`import`"),
        }
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
            ExprKind::Bool(v) => Ok(block(
                "logic_boolean",
                &field("BOOL", &jstr(if *v { "TRUE" } else { "FALSE" })),
                "",
                "",
                "",
            )),
            ExprKind::Str(s) => Ok(block("text", &field("TEXT", &jstr(s)), "", "", "")),
            ExprKind::Name(n) => {
                let id = self.var_id(n);
                Ok(block(
                    "variables_get",
                    &field("VAR", &var_ref(&id)),
                    "",
                    "",
                    "",
                ))
            }
            ExprKind::Unary(UnOp::Not, inner) => Ok(block(
                "logic_negate",
                "",
                &input("BOOL", &self.value_block(inner)?),
                "",
                "",
            )),
            ExprKind::Unary(UnOp::Neg, inner) => {
                if let ExprKind::Int(n) = inner.kind {
                    return Ok(number(-(n as f64)));
                }
                if let ExprKind::Float(f) = inner.kind {
                    return Ok(number(-f));
                }
                // -x as 0 - x.
                let ab = format!(
                    "{},{}",
                    input("A", &number(0.0)),
                    input("B", &self.value_block(inner)?)
                );
                Ok(block(
                    "math_arithmetic",
                    &field("OP", &jstr("MINUS")),
                    &ab,
                    "",
                    "",
                ))
            }
            ExprKind::Bin(op, a, b) => self.bin_block(*op, a, b),
            // A named call used as a value (`double(x)` in `y = double(x)`)
            // becomes the output-shaped call block.
            ExprKind::Call(name, args) => self.call_block(name, args, false, ""),
            // A method call used as a value (`xs.pop()`, `s.upper()`).
            ExprKind::MethodCall(obj, method, args) => {
                self.method_block(obj, method, args, false, "")
            }
            ExprKind::List(items) => {
                // Blockly's `lists_create_with` with N value inputs ADD0..ADDn-1
                // and an extraState item count.
                let mut ins = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    ins.push(input(&format!("ADD{i}"), &self.value_block(item)?));
                }
                let extra = format!("{{\"itemCount\":{}}}", items.len());
                Ok(block("lists_create_with", "", &ins.join(","), &extra, ""))
            }
            ExprKind::Tuple(_) => unsupported("a tuple"),
            // Dict literal `{k: v, ...}` -> python_dict with KEY0/VAL0.. value
            // inputs and a pair count in extraState (a pair mutator backs it).
            ExprKind::Dict(pairs) => {
                let mut ins = Vec::with_capacity(pairs.len() * 2);
                for (i, (k, v)) in pairs.iter().enumerate() {
                    ins.push(input(&format!("KEY{i}"), &self.value_block(k)?));
                    ins.push(input(&format!("VAL{i}"), &self.value_block(v)?));
                }
                let extra = format!("{{\"pairCount\":{}}}", pairs.len());
                Ok(block("python_dict", "", &ins.join(","), &extra, ""))
            }
            // Subscript read `target[index]` (lists, dicts, strings).
            ExprKind::Index(obj, idx) => {
                let t = self.value_block(obj)?;
                let i = self.value_block(idx)?;
                Ok(block(
                    "python_index",
                    "",
                    &format!("{},{}", input("TARGET", &t), input("INDEX", &i)),
                    "",
                    "",
                ))
            }
            ExprKind::Slice { .. } => unsupported("slicing"),
            ExprKind::Attr(..) => unsupported("an attribute"),
            ExprKind::ListComp { .. } | ExprKind::DictComp { .. } => unsupported("a comprehension"),
            ExprKind::NoneLit => unsupported("None"),
            ExprKind::Kwarg(..) => unsupported("a keyword argument"),
        }
    }

    fn bin_block(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<String, String> {
        let av = self.value_block(a)?;
        let bv = self.value_block(b)?;
        let ab = format!("{},{}", input("A", &av), input("B", &bv));
        let arith = |op: &str| block("math_arithmetic", &field("OP", &jstr(op)), &ab, "", "");
        let compare = |op: &str| block("logic_compare", &field("OP", &jstr(op)), &ab, "", "");
        Ok(match op {
            BinOp::Add => arith("ADD"),
            BinOp::Sub => arith("MINUS"),
            BinOp::Mul => arith("MULTIPLY"),
            BinOp::Div | BinOp::FloorDiv => arith("DIVIDE"),
            BinOp::Pow => arith("POWER"),
            BinOp::Mod => block(
                "math_modulo",
                "",
                &format!("{},{}", input("DIVIDEND", &av), input("DIVISOR", &bv)),
                "",
                "",
            ),
            BinOp::Eq => compare("EQ"),
            BinOp::Ne => compare("NEQ"),
            BinOp::Lt => compare("LT"),
            BinOp::Le => compare("LTE"),
            BinOp::Gt => compare("GT"),
            BinOp::Ge => compare("GTE"),
            BinOp::And => block("logic_operation", &field("OP", &jstr("AND")), &ab, "", ""),
            BinOp::Or => block("logic_operation", &field("OP", &jstr("OR")), &ab, "", ""),
            BinOp::In | BinOp::NotIn | BinOp::BitOr | BinOp::BitAnd | BinOp::BitXor => {
                return Err(format!(
                    "line {}: this operator has no block yet (edit it in the text pane)",
                    a.line
                ));
            }
        })
    }

    /// A call to a named function — `python_call_statement` (void call, in a
    /// statement stack) or `python_call_value` (returns a value, has an output).
    /// The function name is a field; the positional arguments are value inputs
    /// `ARG0..ARGn-1`, with the count carried in `extraState` so the custom
    /// block can rebuild that many sockets on load. Keyword arguments have no
    /// block yet.
    fn call_block(
        &mut self,
        name: &str,
        args: &[Expr],
        statement: bool,
        next: &str,
    ) -> Result<String, String> {
        let mut ins = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            if let ExprKind::Kwarg(..) = a.kind {
                return Err(format!(
                    "line {}: a keyword argument has no block yet (edit it in the text pane)",
                    a.line
                ));
            }
            // A None arg renders as an empty socket (see slot_input); argCount
            // below still reserves it, so the socket shows and reads back None.
            let slot = self.slot_input(&format!("ARG{i}"), a)?;
            if !slot.is_empty() {
                ins.push(slot);
            }
        }
        let ty = if statement {
            "python_call_statement"
        } else {
            "python_call_value"
        };
        let extra = format!("{{\"argCount\":{}}}", args.len());
        Ok(block(
            ty,
            &field("NAME", &jstr(name)),
            &ins.join(","),
            &extra,
            next,
        ))
    }

    /// A method call `obj.method(args)` — `python_method_statement` (void, e.g.
    /// `xs.append(5)`) or `python_method_value` (returns a value, e.g.
    /// `xs.pop()`). The receiver is the OBJECT value input, the method name is a
    /// field, and positional args are ARG0..ARGn-1 with the count in extraState
    /// (the same call mutator backs them).
    fn method_block(
        &mut self,
        obj: &Expr,
        method: &str,
        args: &[Expr],
        statement: bool,
        next: &str,
    ) -> Result<String, String> {
        let mut ins = vec![input("OBJECT", &self.value_block(obj)?)];
        for (i, a) in args.iter().enumerate() {
            if let ExprKind::Kwarg(..) = a.kind {
                return Err(format!(
                    "line {}: a keyword argument has no block yet (edit it in the text pane)",
                    a.line
                ));
            }
            let slot = self.slot_input(&format!("ARG{i}"), a)?;
            if !slot.is_empty() {
                ins.push(slot);
            }
        }
        let ty = if statement {
            "python_method_statement"
        } else {
            "python_method_value"
        };
        let extra = format!("{{\"argCount\":{}}}", args.len());
        Ok(block(
            ty,
            &field("METHOD", &jstr(method)),
            &ins.join(","),
            &extra,
            next,
        ))
    }
}

/// Assemble a Blockly JSON block object. `fields` and `inputs` are the
/// comma-joined *bodies* of those objects (built with [`field`]/[`input`]);
/// pass "" to omit. `extra_state` is a raw JSON value or "". `next` is the JSON
/// of the following block in the chain, or "".
fn block(ty: &str, fields: &str, inputs: &str, extra_state: &str, next: &str) -> String {
    let mut p = format!("\"type\":{}", jstr(ty));
    if !extra_state.is_empty() {
        p.push_str(&format!(",\"extraState\":{extra_state}"));
    }
    if !fields.is_empty() {
        p.push_str(&format!(",\"fields\":{{{fields}}}"));
    }
    if !inputs.is_empty() {
        p.push_str(&format!(",\"inputs\":{{{inputs}}}"));
    }
    if !next.is_empty() {
        p.push_str(&format!(",\"next\":{{\"block\":{next}}}"));
    }
    format!("{{{p}}}")
}

/// Which event a hat block represents, with its event-specific datum.
enum HatKind {
    /// `on(selector, "click", handler)` — a click handler on an element.
    Click(String),
    /// `on_key(key, handler)` — a keyboard handler.
    Key(String),
    /// `every(ms, handler)` — a timer / game-loop tick (ms as source text).
    Every(String),
}

/// If `stmts` begins with the event-handler pattern — a zero-argument `def`
/// named N immediately followed by its registration call referring to N
/// (`on(sel, "click", N)`, `on_key(key, N)`, or `every(ms, N)`) — describe the
/// hat: its kind, the handler name, and the def body. The IDE shows that pair as
/// one event-hat block (see `assets/blockly-init.js`), and folding it back keeps
/// blocks and text in sync. Returns `None` when the two statements don't form
/// the exact pattern (so they round-trip as a plain def + call instead).
fn match_event_hat(stmts: &[Stmt]) -> Option<(HatKind, &str, &[Stmt])> {
    let [def_s, reg_s, ..] = stmts else {
        return None;
    };
    let StmtKind::Def {
        name,
        params,
        defaults,
        return_type,
        body,
        ..
    } = &def_s.kind
    else {
        return None;
    };
    // Only a plain zero-arg handler folds into a hat; anything richer (params,
    // defaults, a return type) stays a normal def block.
    if !params.is_empty() || !defaults.is_empty() || return_type.is_some() {
        return None;
    }
    let StmtKind::Expr(call) = &reg_s.kind else {
        return None;
    };
    let ExprKind::Call(reg, args) = &call.kind else {
        return None;
    };
    // The handler argument must be a bare name referring to this def.
    let refers = |e: &Expr| matches!(&e.kind, ExprKind::Name(n) if n == name);
    let str_of = |e: &Expr| match &e.kind {
        ExprKind::Str(s) => Some(s.clone()),
        _ => None,
    };
    let kind = match (reg.as_str(), args.as_slice()) {
        ("on", [sel, ev, h])
            if refers(h) && matches!(&ev.kind, ExprKind::Str(s) if s == "click") =>
        {
            HatKind::Click(str_of(sel)?)
        }
        ("on_key", [key, h]) if refers(h) => HatKind::Key(str_of(key)?),
        ("every", [ms, h]) if refers(h) => HatKind::Every(num_text(ms)?),
        _ => return None,
    };
    Some((kind, name, body))
}

/// Render a simple numeric/name expression to the source text an `every` hat's
/// editable MS field holds. Only the shapes the forward generator can produce
/// (`every(<MS text>, handler)`) need to round-trip; anything else returns
/// `None`, leaving the pair as a plain def + call.
fn num_text(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Int(n) => Some(n.to_string()),
        ExprKind::Float(f) => Some(format!("{f}")),
        ExprKind::Name(n) => Some(n.clone()),
        _ => None,
    }
}

/// Split a `"line N: message"` diagnostic into its 1-based line and the bare
/// message. Returns `(None, whole)` if there's no recognizable prefix.
fn split_line_prefix(msg: &str) -> (Option<usize>, String) {
    if let Some(rest) = msg.strip_prefix("line ")
        && let Some((num, tail)) = rest.split_once(": ")
        && let Ok(n) = num.parse::<usize>()
    {
        return (Some(n), tail.to_string());
    }
    (None, msg.to_string())
}

/// Turn a `stmt_block` error string into a structured note (line + message),
/// taking the line from the message's own `"line N:"` prefix — the failure may
/// come from a statement nested deeper than the one being rendered.
fn note_from(msg: &str) -> CompileError {
    match split_line_prefix(msg) {
        (Some(n), bare) => CompileError::at(n, bare),
        (None, bare) => CompileError::general(bare),
    }
}

/// Link standalone block objects into one connected stack (each becomes the
/// `next` of the one before it). Returns "" for an empty list.
fn stitch(blocks: Vec<String>) -> String {
    let mut iter = blocks.into_iter().rev();
    let Some(mut chain) = iter.next() else {
        return String::new();
    };
    for prev in iter {
        chain = with_next(&prev, &chain);
    }
    chain
}

/// Stitch the current run into a connected stack and push it as a top-level
/// stack. Drains `run`.
fn flush_run(run: &mut Vec<String>, tops: &mut Vec<String>) {
    if !run.is_empty() {
        tops.push(stitch(std::mem::take(run)));
    }
}

/// Tag a block object with its source line in the `data` field, inserted right
/// after the opening `{`. `data` is a standard Blockly per-block string that
/// round-trips through serialization.
fn with_data(block: &str, line: usize) -> String {
    debug_assert!(block.starts_with('{'));
    format!("{{\"data\":\"{line}\",{}", &block[1..])
}

/// Splice `next` into a standalone block object (built with no `next`): insert
/// `,"next":{"block":…}` just before the block's closing brace.
fn with_next(block: &str, next: &str) -> String {
    debug_assert!(block.ends_with('}'));
    format!(
        "{},\"next\":{{\"block\":{next}}}}}",
        &block[..block.len() - 1]
    )
}

/// One `"NAME": <value>` field entry. `value_json` is a raw JSON value.
fn field(name: &str, value_json: &str) -> String {
    format!("{}:{value_json}", jstr(name))
}

/// One `"NAME": { "block": <block> }` input entry.
fn input(name: &str, block_json: &str) -> String {
    format!("{}:{{\"block\":{block_json}}}", jstr(name))
}

/// A FieldVariable value: `{ "id": "var_0" }` (name/type come from the
/// top-level `variables` array).
fn var_ref(id: &str) -> String {
    format!("{{\"id\":{}}}", jstr(id))
}

/// True if `step` is the literal `1` — Python's default — so the loop's step
/// socket can be hidden and the block reads `range(start, stop)`.
fn step_is_one(step: &Expr) -> bool {
    matches!(step.kind, ExprKind::Int(1))
}

/// A `math_number` block. Whole values serialize without a trailing `.0`.
fn number(n: f64) -> String {
    let num = if !n.is_finite() {
        // inf/NaN can't be valid JSON; fall back to 0 rather than emit `inf`.
        "0".to_string()
    } else if n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    };
    // NUM is a bare JSON number, not a string.
    block("math_number", &field("NUM", &num), "", "", "")
}

/// Render a type-annotation expression back to Python source for a block's
/// signature field — `int`, `list[int]`, `dict[str, int]`, `mod.T`. Returns
/// `None` for shapes we don't reconstruct (e.g. a call), so the caller can keep
/// that def in the text pane rather than emit a broken signature.
fn type_to_source(e: &Expr) -> Option<String> {
    Some(match &e.kind {
        ExprKind::Name(n) => n.clone(),
        ExprKind::NoneLit => "None".to_string(),
        ExprKind::Attr(obj, attr) => format!("{}.{attr}", type_to_source(obj)?),
        ExprKind::Index(obj, key) => {
            format!("{}[{}]", type_to_source(obj)?, type_to_source(key)?)
        }
        // `dict[str, int]` subscripts the type with a tuple of args.
        ExprKind::Tuple(items) => {
            let parts: Option<Vec<String>> = items.iter().map(type_to_source).collect();
            parts?.join(", ")
        }
        _ => return None,
    })
}

/// Render a simple *value* expression back to Python source for a default
/// argument (`def f(x=<here>)`). Handles the common K-12 defaults — literals, a
/// name, a negated number, and simple containers — and returns `None` for
/// anything more exotic (a call, an expression, a comprehension), so the caller
/// keeps that def in the text pane rather than emit a broken signature.
fn value_to_source(e: &Expr) -> Option<String> {
    Some(match &e.kind {
        ExprKind::Int(n) => n.to_string(),
        ExprKind::Float(f) if f.is_finite() => {
            if f.fract() == 0.0 {
                format!("{f:.1}") // keep it a float: 1.0, not 1
            } else {
                format!("{f}")
            }
        }
        ExprKind::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        ExprKind::NoneLit => "None".to_string(),
        ExprKind::Str(s) => jstr(s), // JSON string == Python double-quoted literal here
        ExprKind::Name(n) => n.clone(),
        ExprKind::Unary(UnOp::Neg, inner) => format!("-{}", value_to_source(inner)?),
        ExprKind::List(items) => {
            let parts: Option<Vec<String>> = items.iter().map(value_to_source).collect();
            format!("[{}]", parts?.join(", "))
        }
        ExprKind::Tuple(items) => {
            let parts: Option<Vec<String>> = items.iter().map(value_to_source).collect();
            let parts = parts?;
            if parts.len() == 1 {
                format!("({},)", parts[0]) // 1-tuple keeps its trailing comma
            } else {
                format!("({})", parts.join(", "))
            }
        }
        ExprKind::Dict(pairs) => {
            let mut parts = Vec::with_capacity(pairs.len());
            for (k, v) in pairs {
                parts.push(format!("{}: {}", value_to_source(k)?, value_to_source(v)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        _ => return None,
    })
}

/// JSON-encode a string (quotes included).
fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_hat_click_folds_def_plus_registration() {
        // def + on(sel,"click",h) collapses into one python_when_click hat.
        let json =
            to_blockly_json("def grow():\n    flash()\non(\"#box\", \"click\", grow)\n").unwrap();
        assert!(json.contains("\"type\":\"python_when_click\""), "{json}");
        assert!(json.contains("\"SELECTOR\":\"#box\""), "{json}");
        assert!(json.contains("\"NAME\":\"grow\""), "{json}");
        // The handler body rides in the DO stack...
        assert!(json.contains("\"DO\":{\"block\""), "{json}");
        assert!(
            json.contains("\"type\":\"python_call_statement\""),
            "{json}"
        );
        // ...and there's no separate def/registration block left over.
        assert!(!json.contains("\"type\":\"python_def\""), "{json}");
    }

    #[test]
    fn event_hat_key_and_every() {
        let key =
            to_blockly_json("def left():\n    flash()\non_key(\"ArrowLeft\", left)\n").unwrap();
        assert!(key.contains("\"type\":\"python_when_key\""), "{key}");
        assert!(key.contains("\"KEY\":\"ArrowLeft\""), "{key}");

        let every = to_blockly_json("def tick():\n    flash()\nevery(30, tick)\n").unwrap();
        assert!(every.contains("\"type\":\"python_every\""), "{every}");
        assert!(every.contains("\"MS\":\"30\""), "{every}");
    }

    #[test]
    fn event_hat_only_folds_the_exact_pattern() {
        // A def whose name the registration does NOT reference stays unfolded.
        let mismatch =
            to_blockly_json("def grow():\n    flash()\non(\"#box\", \"click\", other)\n").unwrap();
        assert!(mismatch.contains("\"type\":\"python_def\""), "{mismatch}");
        assert!(!mismatch.contains("python_when_click"), "{mismatch}");

        // A handler that takes a parameter isn't a zero-arg hat.
        let with_param =
            to_blockly_json("def grow(x):\n    flash()\non(\"#box\", \"click\", grow)\n").unwrap();
        assert!(
            with_param.contains("\"type\":\"python_def\""),
            "{with_param}"
        );
        assert!(!with_param.contains("python_when_click"), "{with_param}");
    }

    #[test]
    fn event_hat_chains_with_surrounding_statements() {
        // A hat in the middle of a program keeps the statements before and after.
        let json = to_blockly_json(
            "x = 1\ndef grow():\n    flash()\non(\"#box\", \"click\", grow)\nprint(x)\n",
        )
        .unwrap();
        assert!(json.contains("\"type\":\"variables_set\""), "{json}");
        assert!(json.contains("\"type\":\"python_when_click\""), "{json}");
        assert!(json.contains("\"type\":\"text_print\""), "{json}");
    }

    #[test]
    fn assignment_and_print() {
        let json = to_blockly_json("x = 5\nprint(x)").unwrap();
        assert!(json.contains("\"variables\":[{\"name\":\"x\",\"id\":\"var_0\"}]"));
        assert!(json.contains("\"type\":\"variables_set\""));
        assert!(json.contains("\"NUM\":5"));
        assert!(json.contains("\"type\":\"text_print\""));
        // print(x) is chained after the assignment.
        assert!(json.contains("\"next\":{\"block\""));
    }

    #[test]
    fn arithmetic_and_comparison() {
        let json = to_blockly_json("y = 2 + 3 * 4").unwrap();
        assert!(json.contains("\"OP\":\"ADD\""));
        assert!(json.contains("\"OP\":\"MULTIPLY\""));
        let json = to_blockly_json("z = 1 < 2").unwrap();
        assert!(json.contains("\"type\":\"logic_compare\""));
        assert!(json.contains("\"OP\":\"LT\""));
    }

    #[test]
    fn if_while_for() {
        let json = to_blockly_json(
            "if x < 3:\n    print(x)\nelif x < 5:\n    print(1)\nelse:\n    print(2)",
        )
        .unwrap();
        assert!(json.contains("\"type\":\"controls_if\""));
        assert!(json.contains("\"extraState\":{\"elseIfCount\":1,\"hasElse\":true}"));

        let json = to_blockly_json("while x < 10:\n    x = x + 1").unwrap();
        assert!(json.contains("\"type\":\"controls_whileUntil\""));

        // range(1, 5) -> python_for_range mirroring the code: TO stays the
        // exclusive stop 5 (no phantom `- 1`), and the default step of 1 hides
        // the step socket entirely — the block reads `range(1, 5)`.
        let json = to_blockly_json("for i in range(1, 5):\n    print(i)").unwrap();
        assert!(json.contains("\"type\":\"python_for_range\""));
        assert!(
            json.contains("\"TO\":{\"block\":{\"type\":\"math_number\",\"fields\":{\"NUM\":5}}}"),
            "expected exclusive TO of 5: {json}"
        );
        assert!(
            !json.contains("\"BY\""),
            "step of 1 must hide the socket: {json}"
        );
        assert!(
            !json.contains("hasStep"),
            "no step -> no extraState: {json}"
        );
    }

    #[test]
    fn for_descending_range_mirrors_range_stop() {
        // range(10, 0, -1): the block mirrors `range()` exactly — TO is the
        // exclusive stop 0 (no +1), and a non-1 step shows the socket (BY = -1)
        // with extraState flagging the shape.
        let json = to_blockly_json("for i in range(10, 0, -1):\n    print(i)").unwrap();
        assert!(json.contains("\"type\":\"python_for_range\""), "{json}");
        assert!(
            json.contains("\"TO\":{\"block\":{\"type\":\"math_number\",\"fields\":{\"NUM\":0}}}"),
            "expected exclusive TO of 0: {json}"
        );
        assert!(
            json.contains("\"hasStep\":true"),
            "non-1 step needs the socket: {json}"
        );
        assert!(json.contains("\"BY\""), "expected a step socket: {json}");
    }

    #[test]
    fn to_blocks_surfaces_a_typo_span() {
        // A misspelled call → error_spans carries the byte range of the name, so
        // the editor squiggles exactly `pint`.
        let src = "pint(\"hi\")\n";
        let out = to_blocks(src);
        assert!(
            out.error_spans.iter().any(|&(s, e)| &src[s..e] == "pint"),
            "spans: {:?}",
            out.error_spans
        );
    }

    #[test]
    fn booleans_and_logic() {
        let json = to_blockly_json("ok = True and not False").unwrap();
        assert!(json.contains("\"type\":\"logic_boolean\""));
        assert!(json.contains("\"type\":\"logic_operation\""));
        assert!(json.contains("\"type\":\"logic_negate\""));
    }

    #[test]
    fn strings_are_not_xml_escaped() {
        // JSON carries `<`, `>`, `&` literally — only JSON metacharacters escape.
        let json = to_blockly_json("print(\"a < b & c\")").unwrap();
        assert!(json.contains("\"TEXT\":\"a < b & c\""), "{json}");
    }

    #[test]
    fn json_string_escaping() {
        assert_eq!(jstr("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(jstr("line\n\t"), "\"line\\n\\t\"");
    }

    #[test]
    fn unsupported_constructs_error_gracefully() {
        let err = to_blockly_json("t = (1, 2)").unwrap_err();
        assert!(err.contains("tuple"), "{err}");
        let err = to_blockly_json("class C:\n    pass").unwrap_err();
        assert!(err.contains("class"), "{err}");
    }

    #[test]
    fn lists_and_for_each() {
        // List literal -> lists_create_with with item count + ADD inputs.
        let json = to_blockly_json("xs = [1, 2, 3]").unwrap();
        assert!(json.contains("\"type\":\"lists_create_with\""), "{json}");
        assert!(json.contains("\"itemCount\":3"), "{json}");
        assert!(json.contains("\"ADD0\""), "{json}");
        assert!(json.contains("\"ADD2\""), "{json}");

        // `for x in xs:` -> controls_forEach with VAR + LIST + DO.
        let json = to_blockly_json("for x in xs:\n    print(x)").unwrap();
        assert!(json.contains("\"type\":\"controls_forEach\""), "{json}");
        assert!(json.contains("\"LIST\""), "{json}");
        assert!(json.contains("\"type\":\"text_print\""), "{json}");
    }

    #[test]
    fn function_def_call_and_return() {
        // def with a body and a return value -> python_def + python_return,
        // PARAMS is the comma-joined name list.
        let json = to_blockly_json("def double(n):\n    return n * 2").unwrap();
        assert!(json.contains("\"type\":\"python_def\""), "{json}");
        assert!(json.contains("\"NAME\":\"double\""), "{json}");
        assert!(json.contains("\"PARAMS\":\"n\""), "{json}");
        assert!(json.contains("\"type\":\"python_return\""), "{json}");
        assert!(json.contains("\"VALUE\""), "{json}");

        // A bare return (no value) emits the block with no VALUE input.
        let json = to_blockly_json("def f():\n    return").unwrap();
        assert!(json.contains("\"type\":\"python_return\""), "{json}");

        // A value call -> python_call_value with ARG inputs + an argCount.
        let json = to_blockly_json("y = double(21)").unwrap();
        assert!(json.contains("\"type\":\"python_call_value\""), "{json}");
        assert!(json.contains("\"NAME\":\"double\""), "{json}");
        assert!(json.contains("\"argCount\":1"), "{json}");
        assert!(json.contains("\"ARG0\""), "{json}");

        // A void call statement -> python_call_statement.
        let json = to_blockly_json("greet(\"Bo\", 3)").unwrap();
        assert!(
            json.contains("\"type\":\"python_call_statement\""),
            "{json}"
        );
        assert!(json.contains("\"argCount\":2"), "{json}");
        assert!(json.contains("\"ARG1\""), "{json}");
    }

    #[test]
    fn function_default_arg_rides_in_params() {
        // A default value folds into the free-text PARAMS field and round-trips
        // through the generator — no dedicated block needed. Unannotated default:
        let j1 = to_blockly_json("def f(x=None):\n    return x").unwrap();
        assert!(j1.contains("\"type\":\"python_def\""), "{j1}");
        assert!(j1.contains("\"PARAMS\":\"x=None\""), "{j1}");
        // Annotated default gets PEP-8 spacing around `=`.
        let j2 = to_blockly_json("def f(n: int = 0):\n    return n").unwrap();
        assert!(j2.contains("\"PARAMS\":\"n: int = 0\""), "{j2}");
        // A string default renders (and stays a def block, not an error).
        let j3 = to_blockly_json("def greet(name=\"friend\"):\n    return name").unwrap();
        assert!(
            j3.contains("\"type\":\"python_def\"") && j3.contains("friend"),
            "{j3}"
        );
        // A default we can't render (a call) keeps the whole def in text.
        let err = to_blockly_json("def f(x=make()):\n    return x").unwrap_err();
        assert!(err.contains("default value"), "{err}");
    }

    #[test]
    fn pass_body_renders_as_an_empty_block() {
        // `def f(): pass` -> a def block with an EMPTY body; the JS generator
        // re-emits `pass` for an empty body, so it round-trips. `pass` produces
        // no block of its own and no "no block yet" note.
        let out = to_blocks("def f():\n    pass\n");
        assert!(out.json.contains("\"type\":\"python_def\""), "{}", out.json);
        assert!(
            out.errors.is_empty(),
            "pass must not be a note: {:?}",
            out.errors
        );
        assert!(
            !out.json.contains("controls_flow"),
            "pass should not render a block: {}",
            out.json
        );
    }

    #[test]
    fn function_type_annotation_becomes_a_typed_block() {
        // Typed surfaces: a `: T` annotation folds inline into PARAMS and a
        // `-> T` rides in extraState, so the signature round-trips faithfully.
        let json = to_blockly_json("def double(n: int) -> int:\n    return n * 2").unwrap();
        assert!(json.contains("\"type\":\"python_def\""), "{json}");
        assert!(json.contains("\"PARAMS\":\"n: int\""), "{json}");
        assert!(json.contains("\"returns\":\"int\""), "{json}");

        // A subscripted type (list[int]) and a param-only annotation also work.
        let json = to_blockly_json("def total(xs: list[int]):\n    return xs").unwrap();
        assert!(json.contains("\"PARAMS\":\"xs: list[int]\""), "{json}");
        assert!(
            !json.contains("\"returns\""),
            "no return annotation: {json}"
        );

        // The untyped form is unchanged (no regression).
        let json = to_blockly_json("def double(n):\n    return n * 2").unwrap();
        assert!(json.contains("\"PARAMS\":\"n\""), "{json}");
        assert!(!json.contains("\"returns\""), "{json}");
    }

    #[test]
    fn unreconstructable_type_annotation_stays_in_text() {
        // A type shape we don't render (here a call `int()`) keeps the def in the
        // text pane rather than emitting a broken signature.
        let err = to_blockly_json("def f(x: int()) -> int:\n    return x").unwrap_err();
        assert!(err.contains("type annotation"), "{err}");
    }

    #[test]
    fn to_blocks_annotated_def_becomes_a_block_with_no_errors() {
        // An annotated def is valid Python and now has a typed block — so it
        // renders cleanly with no diagnostics at all.
        let out = to_blocks("def double(n: int) -> int:\n    return n * 2\n");
        assert!(out.json.contains("\"type\":\"python_def\""), "{}", out.json);
        assert!(out.json.contains("\"PARAMS\":\"n: int\""), "{}", out.json);
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(out.error_lines.is_empty());
    }

    #[test]
    fn statements_carry_their_source_line_in_data() {
        // Each statement block is tagged with its 1-based line (for the debugger
        // block highlight); nested body statements get their own line too.
        let json = to_blockly_json("x = 1\nfor i in range(3):\n    print(i)\n").unwrap();
        assert!(json.contains("\"data\":\"1\""), "x= on line 1: {json}");
        assert!(json.contains("\"data\":\"2\""), "for on line 2: {json}");
        assert!(json.contains("\"data\":\"3\""), "print on line 3: {json}");
    }

    #[test]
    fn indexing_read_and_assignment() {
        // Subscript read `xs[0]` -> python_index with TARGET + INDEX.
        let json = to_blockly_json("y = xs[0]").unwrap();
        assert!(json.contains("\"type\":\"python_index\""), "{json}");
        assert!(json.contains("\"TARGET\""), "{json}");
        assert!(json.contains("\"INDEX\""), "{json}");

        // Subscript assignment `xs[i] = 9` -> python_set_index.
        let json = to_blockly_json("xs[i] = 9").unwrap();
        assert!(json.contains("\"type\":\"python_set_index\""), "{json}");
        assert!(json.contains("\"VALUE\""), "{json}");

        // Dict-style key access round-trips through the same blocks.
        let json = to_blockly_json("v = scores[\"ann\"]").unwrap();
        assert!(json.contains("\"type\":\"python_index\""), "{json}");
        assert!(json.contains("\"TEXT\":\"ann\""), "{json}");
    }

    #[test]
    fn dict_literal_round_trips() {
        // `{"a": 1, "b": 2}` -> python_dict with KEY/VAL inputs + pair count.
        let json = to_blockly_json("d = {\"a\": 1, \"b\": 2}").unwrap();
        assert!(json.contains("\"type\":\"python_dict\""), "{json}");
        assert!(json.contains("\"pairCount\":2"), "{json}");
        assert!(json.contains("\"KEY0\""), "{json}");
        assert!(json.contains("\"VAL1\""), "{json}");
        assert!(json.contains("\"TEXT\":\"a\""), "{json}");

        // An empty dict is representable too (pairCount 0).
        let json = to_blockly_json("d = {}").unwrap();
        assert!(json.contains("\"type\":\"python_dict\""), "{json}");
        assert!(json.contains("\"pairCount\":0"), "{json}");
    }

    #[test]
    fn method_calls_statement_and_value() {
        // Void method call `xs.append(5)` -> python_method_statement.
        let json = to_blockly_json("xs.append(5)").unwrap();
        assert!(
            json.contains("\"type\":\"python_method_statement\""),
            "{json}"
        );
        assert!(json.contains("\"METHOD\":\"append\""), "{json}");
        assert!(json.contains("\"OBJECT\""), "{json}");
        assert!(json.contains("\"argCount\":1"), "{json}");

        // Value method call `last = xs.pop()` -> python_method_value, 0 args.
        let json = to_blockly_json("last = xs.pop()").unwrap();
        assert!(json.contains("\"type\":\"python_method_value\""), "{json}");
        assert!(json.contains("\"METHOD\":\"pop\""), "{json}");
        assert!(json.contains("\"argCount\":0"), "{json}");
    }

    #[test]
    fn break_and_continue() {
        let json = to_blockly_json("while True:\n    if x:\n        break\n    continue").unwrap();
        assert!(json.contains("\"FLOW\":\"BREAK\""));
        assert!(json.contains("\"FLOW\":\"CONTINUE\""));
    }

    #[test]
    fn to_blocks_clean_program_has_no_errors() {
        let out = to_blocks("x = 5\nprint(x)\n");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(out.error_lines.is_empty());
        assert!(out.json.contains("\"type\":\"variables_set\""));
        assert!(out.json.contains("\"type\":\"text_print\""));
    }

    #[test]
    fn to_blocks_recovers_from_a_syntax_error() {
        // Missing colon: the parser drops the broken loop, but the assignment
        // before and the print after still render, with one diagnostic that is
        // highlightable on line 2.
        let out =
            to_blocks("total = 0\nfor i in range(1, 6)\n    total = total + 1\nprint(total)\n");
        assert!(out.json.contains("\"type\":\"variables_set\""));
        assert!(out.json.contains("\"type\":\"text_print\""));
        assert_eq!(out.errors.len(), 1, "{:?}", out.errors);
        assert!(out.errors[0].to_string().contains("colon"));
        assert_eq!(out.error_lines, vec![2], "syntax error should mark line 2");
    }

    #[test]
    fn to_blocks_flags_typod_function_call() {
        // `pint` is a typo for `print`: a clear "did you mean" message that IS
        // highlightable, and the vaguer "no block yet" note is suppressed.
        let out = to_blocks("print(\"Hello\")\nfor i in range(1, 4):\n    pint(i)\n");
        assert!(
            out.errors
                .iter()
                .any(|e| e.to_string().contains("did you mean `print`")),
            "{:?}",
            out.errors
        );
        assert!(
            !out.errors
                .iter()
                .any(|e| e.to_string().contains("no block yet")),
            "redundant note should be suppressed: {:?}",
            out.errors
        );
        assert_eq!(out.error_lines, vec![3], "typo on line 3 is highlightable");
    }

    #[test]
    fn to_blocks_recovers_inside_a_compound_body() {
        // Per-block recovery: the `for` still renders (with the printable line in
        // its body) even though one body line (a tuple) can't be a block yet.
        let out = to_blocks("for i in range(3):\n    print(i)\n    y = (1, 2)\n");
        assert!(
            out.json.contains("\"type\":\"python_for_range\""),
            "{}",
            out.json
        );
        assert!(out.json.contains("\"type\":\"text_print\""), "{}", out.json);
        assert!(
            out.errors.iter().any(|e| e.to_string().contains("tuple")),
            "{:?}",
            out.errors
        );
        assert!(out.error_lines.is_empty(), "a tuple isn't a syntax error");
    }

    #[test]
    fn to_blocks_skips_unsupported_but_keeps_the_rest() {
        // A tuple has no block yet; `x` and `z` still render as two separate
        // stacks, and the tuple is reported as a NOTE — but it's valid Python,
        // so it must NOT be highlighted as a syntax error.
        let out = to_blocks("x = 1\ny = (1, 2)\nz = 3\n");
        assert!(out.json.contains("\"NUM\":1"), "{}", out.json); // x
        assert!(out.json.contains("\"NUM\":3"), "{}", out.json); // z
        assert!(
            out.errors.iter().any(|e| e.to_string().contains("tuple")),
            "{:?}",
            out.errors
        );
        assert!(
            out.error_lines.is_empty(),
            "valid-but-unrepresentable code must not be flagged: {:?}",
            out.error_lines
        );
    }
}

#[cfg(test)]
mod none_roundtrip_tests {
    use super::*;

    /// The generator's rule is "empty socket reads back as None", so the
    /// decompiler must render `None` as that same empty socket — never drop
    /// the statement. Dropping it made half-built blocks vanish from the
    /// canvas when the debounced text->blocks reload fired mid-edit.
    #[test]
    fn none_args_round_trip_as_empty_sockets() {
        let out = to_blocks("set_position(None, None, None)\n");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        // The call block survives, reserves all three sockets, fills none.
        assert!(out.json.contains("\"NAME\":\"set_position\""), "{}", out.json);
        assert!(out.json.contains("\"argCount\":3"), "{}", out.json);
        assert!(!out.json.contains("ARG0"), "{}", out.json);
        // Partially filled: the filled socket stays, the empty one is omitted.
        let out = to_blocks("set_position(\"#box\", None, 40)\n");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(out.json.contains("ARG0"), "{}", out.json);
        assert!(!out.json.contains("ARG1"), "{}", out.json);
        assert!(out.json.contains("ARG2"), "{}", out.json);
    }

    #[test]
    fn none_in_print_and_assign_round_trip() {
        let out = to_blocks("print(None)\nx = None\n");
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert!(out.json.contains("text_print"), "{}", out.json);
        assert!(out.json.contains("variables_set"), "{}", out.json);
    }
}
