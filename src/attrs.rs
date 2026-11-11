//! Fixed attribute sets: an instance's attributes are the ones its class
//! gives it, so `self.speeeed = 5` is a mistake the compiler names instead of
//! a new attribute nobody reads (docs/CLASS_LAYOUT.md, step 1).
//!
//! A class's attributes are its class variables, the attributes its
//! `__init__` sets on `self` (anywhere in `__init__`, including inside `if`
//! and loops), and everything its base class has. Assigning any other
//! attribute is a compile error on every surface. CPython accepts such a
//! program, so this is a subset rule: every program p2w accepts is still
//! valid Python. It is the rule CPython itself applies to a class with
//! `__slots__`, with the same kind of error (AttributeError), and it is what
//! lets each class get a fixed layout.
//!
//! A class that declares `__slots__` as a tuple or list of names gets exactly
//! those, plus its class variables: even its `__init__` can't set another,
//! as in CPython.

use crate::ast::{ExprKind, Stmt, StmtKind};
use crate::error::{CompileError, ErrorKind};
use crate::lint::for_each_child_block;
use std::collections::{BTreeMap, BTreeSet};

/// Each class's base and the attributes the class itself introduces (class
/// variables, what its `__init__` sets on `self`, or its `__slots__`).
type OwnSets<'a> = BTreeMap<&'a str, (Option<&'a str>, BTreeSet<String>)>;

fn own_sets(program: &[Stmt]) -> OwnSets<'_> {
    let mut own: OwnSets = BTreeMap::new();
    for s in program {
        if let StmtKind::ClassDef {
            name,
            base,
            methods,
            class_vars,
        } = &s.kind
        {
            let mut set: BTreeSet<String> = class_vars.iter().map(|(n, _)| n.clone()).collect();
            if let Some(slots) = declared_slots(class_vars) {
                set.extend(slots);
                own.insert(name, (base.as_deref(), set));
                continue;
            }
            for m in methods.iter().filter(|m| m.name == "__init__") {
                if let Some(this) = m.params.first() {
                    each_attr_target(&m.body, &mut |obj, attr, _| {
                        if obj == Some(this.as_str()) {
                            set.insert(attr.to_string());
                        }
                        Ok(())
                    })
                    .ok();
                }
            }
            own.insert(name, (base.as_deref(), set));
        }
    }
    own
}

/// Every class's attribute set, inherited ones included. `None` for a class
/// whose base isn't a class in this program: its attributes can't be known,
/// so nothing about it is checked.
fn attribute_sets(program: &[Stmt]) -> BTreeMap<String, Option<BTreeSet<String>>> {
    let own = own_sets(program);
    let mut out = BTreeMap::new();
    for name in own.keys() {
        // Walk the base chain; a base outside the program (or a cycle,
        // which the class pass rejects anyway) leaves the class unknown.
        let mut set = BTreeSet::new();
        let mut cur = Some(*name);
        let mut seen = BTreeSet::new();
        let known = loop {
            let Some(c) = cur else { break true };
            if !seen.insert(c) {
                break false;
            }
            let Some((base, attrs)) = own.get(c) else {
                break false;
            };
            set.extend(attrs.iter().cloned());
            cur = *base;
        };
        out.insert(name.to_string(), known.then_some(set));
    }
    out
}

/// Each class's slot layout, for backends that give objects fixed fields
/// (docs/CLASS_LAYOUT.md): the base's slots first, in the base's order, so an
/// inherited attribute sits at the same index in every subclass; then the
/// class's own new attributes, sorted. `__slots__` itself takes no slot.
/// Classes whose base chain leaves the program are left out.
pub(crate) fn slot_layouts(program: &[Stmt]) -> BTreeMap<String, Vec<String>> {
    fn layout<'a>(
        name: &'a str,
        own: &OwnSets<'a>,
        out: &mut BTreeMap<String, Vec<String>>,
        depth: usize,
    ) -> Option<Vec<String>> {
        if let Some(done) = out.get(name) {
            return Some(done.clone());
        }
        if depth > own.len() {
            return None; // a cycle; the class pass rejects it
        }
        let (base, attrs) = own.get(name)?;
        let mut slots = match base {
            Some(b) => layout(b, own, out, depth + 1)?,
            None => Vec::new(),
        };
        for a in attrs {
            if a != "__slots__" && !slots.contains(a) {
                slots.push(a.clone());
            }
        }
        out.insert(name.to_string(), slots.clone());
        Some(slots)
    }
    let own = own_sets(program);
    let mut out = BTreeMap::new();
    for name in own.keys() {
        layout(name, &own, &mut out, 0);
    }
    out
}

/// The names in `__slots__ = ("x", "y")` (a tuple or list of strings), if the
/// class declares it that way.
fn declared_slots(class_vars: &[(String, crate::ast::Expr)]) -> Option<Vec<String>> {
    let (_, value) = class_vars.iter().find(|(n, _)| n == "__slots__")?;
    let items = match &value.kind {
        ExprKind::Tuple(items) | ExprKind::List(items) => items,
        ExprKind::Str(one) => return Some(vec![one.clone()]),
        _ => return None,
    };
    items
        .iter()
        .map(|i| match &i.kind {
            ExprKind::Str(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// Call `f(object_name, attribute, line)` for every attribute assignment
/// target in `stmts` and the blocks inside them: `x.a = …`, `x.a += …` and
/// `x.a, y = …`. `object_name` is the target's object when it is a plain name.
fn each_attr_target<'a>(
    stmts: &'a [Stmt],
    f: &mut dyn FnMut(Option<&'a str>, &'a str, &'a Stmt) -> Result<(), CompileError>,
) -> Result<(), CompileError> {
    for s in stmts {
        match &s.kind {
            StmtKind::SetAttr { obj, attr, .. } => {
                let name = match &obj.kind {
                    ExprKind::Name(n) => Some(n.as_str()),
                    _ => None,
                };
                f(name, attr, s)?;
            }
            StmtKind::UnpackAssign { targets, .. } => {
                for t in targets {
                    if let ExprKind::Attr(obj, attr) = &t.kind {
                        let name = match &obj.kind {
                            ExprKind::Name(n) => Some(n.as_str()),
                            _ => None,
                        };
                        f(name, attr, s)?;
                    }
                }
            }
            _ => {}
        }
        let mut res = Ok(());
        for_each_child_block(s, |block, _| {
            if res.is_ok() {
                res = each_attr_target(block, f);
            }
        });
        res?;
    }
    Ok(())
}

fn error(s: &Stmt, message: String) -> CompileError {
    let mut e = CompileError::at(s.line, message).with_kind(ErrorKind::Attribute);
    if s.span != (0, 0) {
        e.span = Some(s.span);
    }
    e
}

fn did_you_mean(attr: &str, set: &BTreeSet<String>) -> String {
    let names: Vec<&str> = set.iter().map(String::as_str).collect();
    match crate::parser::did_you_mean(attr, &names) {
        Some(m) => format!(" Did you mean `{m}`?"),
        None => String::new(),
    }
}

/// The check every surface runs before compiling (see `hoist`).
pub(crate) fn check_attribute_sets(program: &[Stmt]) -> Result<(), CompileError> {
    let sets = attribute_sets(program);
    // Every attribute any class has: an assignment to a name outside it can
    // never be legal, whatever the object is.
    let any: BTreeSet<String> = sets.values().flatten().flatten().cloned().collect();

    for s in program {
        if let StmtKind::ClassDef {
            name,
            methods,
            class_vars,
            ..
        } = &s.kind
        {
            let Some(Some(set)) = sets.get(name) else {
                continue; // a base outside the program: unknown attributes
            };
            // With `__slots__`, even `__init__` is held to the declared names.
            let slotted = declared_slots(class_vars).is_some();
            for m in methods.iter().filter(|m| slotted || m.name != "__init__") {
                let Some(this) = m.params.first() else {
                    continue;
                };
                each_attr_target(&m.body, &mut |obj, attr, st| {
                    if obj == Some(this.as_str()) && !set.contains(attr) {
                        return Err(error(
                            st,
                            format!(
                                "`{name}` has no attribute `{attr}` to set: {}{}",
                                if slotted {
                                    format!(
                                        "its `__slots__` lists every attribute `{name}` objects have."
                                    )
                                } else {
                                    format!(
                                        "a `{name}` gets its attributes in `__init__`, so `{}` \
                                         can only change those.",
                                        m.name
                                    )
                                },
                                match did_you_mean(attr, set).as_str() {
                                    _ if slotted => format!(
                                        "{} Otherwise add `'{attr}'` to `__slots__`.",
                                        did_you_mean(attr, set)
                                    ),
                                    "" => format!(
                                        " To give every `{name}` a `{attr}`, set `self.{attr}` in \
                                         `__init__`."
                                    ),
                                    hint => hint.to_string(),
                                }
                            ),
                        ));
                    }
                    Ok(())
                })?;
            }
        }
    }

    // Outside a class's own methods the object's class isn't known here, so
    // only an attribute no class has at all is certainly a mistake.
    if sets.values().any(Option::is_none) {
        return Ok(()); // a class with unknown attributes could have it
    }
    let outside: Vec<&Stmt> = program
        .iter()
        .filter(|s| !matches!(s.kind, StmtKind::ClassDef { .. }))
        .collect();
    for s in outside {
        each_attr_target(std::slice::from_ref(s), &mut |obj, attr, st| {
            // `Counter.made = …` sets a class variable, which the class
            // pass checks with its own words; this rule is about instances.
            if any.contains(attr) || obj.is_some_and(|o| sets.contains_key(o)) {
                return Ok(());
            }
            Err(error(
                st,
                if any.is_empty() {
                    format!(
                        "nothing in this program has an attribute `{attr}` to set: attributes \
                         belong to a class, which gives them out in its `__init__`."
                    )
                } else {
                    format!(
                        "no class in this program has an attribute `{attr}` to set: a class \
                         gives its objects their attributes in `__init__`.{}",
                        did_you_mean(attr, &any)
                    )
                },
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(src: &str) -> Result<(), String> {
        let tokens = crate::lexer::lex(src).map_err(|e| e.message)?;
        let program = crate::parser::parse(&tokens).map_err(|e| e.message)?;
        check_attribute_sets(&program).map_err(|e| e.message)
    }

    /// The classic typo: a method sets an attribute `__init__` never gave
    /// the object. Named, with the likely intended attribute.
    #[test]
    fn a_misspelt_attribute_in_a_method_is_named() {
        let err = check(
            "class Robot:\n    def __init__(self):\n        self.speed = 50\n    def boost(self):\n        self.speeed = 100\n",
        )
        .unwrap_err();
        assert!(
            err.contains("`Robot` has no attribute `speeed` to set"),
            "{err}"
        );
        assert!(err.contains("Did you mean `speed`?"), "{err}");
        // Not a typo of anything: say where attributes come from.
        let err = check(
            "class Robot:\n    def __init__(self):\n        self.speed = 50\n    def arm(self):\n        self.armed = True\n",
        )
        .unwrap_err();
        assert!(err.contains("set `self.armed` in `__init__`"), "{err}");
    }

    #[test]
    fn programs_that_follow_the_rule_compile() {
        for src in [
            // Set in __init__, inside an if; changed later, including +=.
            "class R:\n    def __init__(self, fast):\n        if fast:\n            self.speed = 100\n        else:\n            self.speed = 10\n    def slow(self):\n        self.speed -= 5\n",
            // A class variable may be set through an instance.
            "class R:\n    count = 0\n    def bump(self):\n        self.count = 1\n",
            // Inherited attributes, and a subclass's __init__ adding its own.
            "class A:\n    def __init__(self):\n        self.x = 1\nclass B(A):\n    def __init__(self):\n        super().__init__()\n        self.y = 2\n    def m(self):\n        self.x = 3\n        self.y = 4\n",
            // Unpacking into attributes.
            "class P:\n    def __init__(self):\n        self.a, self.b = 1, 2\n    def swap(self):\n        self.a, self.b = self.b, self.a\n",
            // Outside a class: an attribute some class has.
            "class R:\n    def __init__(self):\n        self.speed = 1\nr = R()\nr.speed = 5\n",
            // A class variable set through the class name is the class pass's
            // business, not this rule's.
            "class C:\n    made = 0\n    def __init__(self):\n        C.made = C.made + 1\n",
        ] {
            assert_eq!(check(src), Ok(()), "{src}");
        }
    }

    #[test]
    fn outside_a_class_an_attribute_no_class_has_is_named() {
        let err = check(
            "class R:\n    def __init__(self):\n        self.speed = 1\nr = R()\nr.speeed = 5\n",
        )
        .unwrap_err();
        assert!(
            err.contains("no class in this program has an attribute `speeed`"),
            "{err}"
        );
        assert!(err.contains("Did you mean `speed`?"), "{err}");
    }

    /// Declared `__slots__` are the attribute set, as in CPython: even
    /// `__init__` can't set another.
    #[test]
    fn declared_slots_hold_init_to_them_too() {
        let err = check(
            "class A:\n    __slots__ = ('x',)\n    def __init__(self):\n        self.x = 1\n        self.y = 2\n",
        )
        .unwrap_err();
        assert!(err.contains("`A` has no attribute `y`"), "{err}");
        assert!(err.contains("add `'y'` to `__slots__`"), "{err}");
        assert_eq!(
            check(
                "class A:\n    __slots__ = ['x', 'y']\n    def __init__(self):\n        self.x = 1\n    def later(self):\n        self.y = 2\n"
            ),
            Ok(())
        );
    }

    /// Inherited attributes keep their index in every subclass.
    #[test]
    fn slot_layouts_put_the_base_first() {
        let src = "class A:\n    k = 0\n    def __init__(self):\n        self.z = 1\n        self.b = 2\nclass B(A):\n    def __init__(self):\n        self.y = 3\n        self.b = 4\nclass C:\n    __slots__ = ('q', 'p')\n";
        let tokens = crate::lexer::lex(src).unwrap();
        let program = crate::parser::parse(&tokens).unwrap();
        let l = slot_layouts(&program);
        assert_eq!(l["A"], vec!["b", "k", "z"]);
        assert_eq!(l["B"], vec!["b", "k", "z", "y"]);
        assert_eq!(l["C"], vec!["p", "q"]);
    }

    /// A method of the base can't set what only a subclass's __init__ gives.
    #[test]
    fn a_base_method_only_has_the_base_attributes() {
        let err = check(
            "class A:\n    def __init__(self):\n        self.x = 1\n    def m(self):\n        self.y = 2\nclass B(A):\n    def __init__(self):\n        self.y = 0\n",
        )
        .unwrap_err();
        assert!(err.contains("`A` has no attribute `y`"), "{err}");
    }
}
