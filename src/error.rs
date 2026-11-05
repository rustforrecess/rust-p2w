//! Compile errors: a friendly student-facing message plus the source line it
//! came from. Every stage (lexer, parser, codegen) produces these; the public
//! string API formats them as `line N: message`.

use std::fmt;

/// What KIND of mistake this is, using CPython's own exception names so the
/// label a student learns here transfers to real Python. It's also the class
/// an assessment stream keys on, such as Jadud's Error Quotient.
///
/// Two levels: every kind is a real CPython exception, except `NotSupported`,
/// and [`family`](Self::family) folds them into the small set a young student
/// learns first. Python's IndentationError is a SyntaxError subclass, so
/// indentation problems are `Syntax` here too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ErrorKind {
    /// The text doesn't parse.
    #[default]
    Syntax,
    /// A name is used that isn't defined anywhere.
    Name,
    /// An operation applied to a value of the wrong type.
    Type,
    /// A module or object has no attribute by that name.
    Attribute,
    /// A module that can't be found.
    Import,
    /// The right type of value, but not one that works here.
    Value,
    /// Asking for an item past the end.
    Index,
    /// Valid Python that this subset doesn't run. Has no CPython equivalent,
    /// because CPython would accept the program.
    NotSupported,
}

impl ErrorKind {
    pub const ALL: [ErrorKind; 8] = [
        ErrorKind::Syntax,
        ErrorKind::Name,
        ErrorKind::Type,
        ErrorKind::Attribute,
        ErrorKind::Import,
        ErrorKind::Value,
        ErrorKind::Index,
        ErrorKind::NotSupported,
    ];

    /// The kid-facing kind this belongs to: a name that couldn't be found,
    /// whether a variable, an attribute or a module, is a `Name` mistake.
    pub fn family(self) -> ErrorKind {
        match self {
            ErrorKind::Attribute | ErrorKind::Import => ErrorKind::Name,
            ErrorKind::Index => ErrorKind::Value,
            other => other,
        }
    }

    /// The exception CPython would raise. `None` for `NotSupported`.
    pub fn python_name(self) -> Option<&'static str> {
        match self {
            ErrorKind::Syntax => Some("SyntaxError"),
            ErrorKind::Name => Some("NameError"),
            ErrorKind::Type => Some("TypeError"),
            ErrorKind::Attribute => Some("AttributeError"),
            ErrorKind::Import => Some("ModuleNotFoundError"),
            ErrorKind::Value => Some("ValueError"),
            ErrorKind::Index => Some("IndexError"),
            ErrorKind::NotSupported => None,
        }
    }

    /// The student-facing family label.
    pub fn headline(self) -> &'static str {
        match self.family() {
            ErrorKind::Syntax => "Syntax Error",
            ErrorKind::Name => "Name Error",
            ErrorKind::Type => "Type Error",
            ErrorKind::Value => "Value Error",
            _ => "Not Supported Here",
        }
    }

    /// The headline, plus the exact CPython name when the family hides it.
    pub fn title(self) -> String {
        match self.python_name() {
            Some(name) if self != self.family() => format!("{} ({name})", self.headline()),
            _ => self.headline().to_string(),
        }
    }

    /// One plain sentence saying what this kind of mistake means.
    pub fn explain(self) -> &'static str {
        match self {
            ErrorKind::Syntax => "Python couldn't read this the way it's written.",
            ErrorKind::Name => "Python doesn't know this name yet.",
            ErrorKind::Type => "This is the wrong type of value for what you're doing.",
            ErrorKind::Attribute => "This thing doesn't have a part with that name.",
            ErrorKind::Import => "Python can't find a module with that name.",
            ErrorKind::Value => "This is the right type of value, but it won't work here.",
            ErrorKind::Index => "There aren't that many items.",
            ErrorKind::NotSupported => "This is real Python, but it doesn't run here yet.",
        }
    }
}

/// What a mistake is about, as data. Wording is generated from this rather
/// than stored at the error site, so a better explainer can replace the
/// wording without touching any site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    /// `called` means it was used as `name(...)`.
    UnknownName {
        name: String,
        suggestion: Option<String>,
        called: bool,
    },
    /// `owner.part`, where `owner` is real but has no such part.
    MissingPart {
        owner: String,
        part: String,
        suggestion: Option<String>,
    },
    UnknownModule {
        name: String,
        available: Vec<String>,
    },
    MissingArgument {
        function: String,
        param: String,
    },
    TooManyArguments {
        function: String,
        max: usize,
    },
}

impl Fact {
    /// Say what happened, in kid words and the student's own names.
    pub fn explanation(&self) -> String {
        match self {
            Fact::UnknownName { name, .. } => format!("Python doesn't know `{name}` yet."),
            Fact::MissingPart { owner, part, .. } => {
                format!("`{owner}` doesn't have a part called `{part}`.")
            }
            Fact::UnknownModule { name, .. } => format!("There's no module called `{name}` here."),
            Fact::MissingArgument { function, param } => {
                format!(
                    "`{function}()` needs a value for `{param}`, but this call doesn't give one."
                )
            }
            Fact::TooManyArguments { function, max } => {
                let values = if *max == 1 { "value" } else { "values" };
                format!("`{function}()` takes {max} {values}, but this call gives more.")
            }
        }
    }

    /// Say what to try next.
    pub fn next_step(&self) -> String {
        match self {
            Fact::UnknownName {
                suggestion: Some(s),
                ..
            } => format!("Did you mean `{s}`?"),
            Fact::UnknownName {
                name, called: true, ..
            } => format!("Check the spelling, or write `def {name}():` above this line."),
            Fact::UnknownName { name, .. } => {
                format!("Check the spelling, or set it above this line, like `{name} = 0`.")
            }
            Fact::MissingPart {
                owner,
                suggestion: Some(s),
                ..
            } => format!("Did you mean `{owner}.{s}`?"),
            Fact::MissingPart { .. } => "Check the spelling after the dot.".to_string(),
            Fact::UnknownModule { available, .. } => {
                format!("You can use: {}.", available.join(", "))
            }
            Fact::MissingArgument { function, .. } => {
                format!("Put a value inside the parentheses of `{function}()`.")
            }
            Fact::TooManyArguments { function, .. } => {
                format!("Take out the extra values, or add parameters to `def {function}`.")
            }
        }
    }

    /// `owner.part`, suggesting the closest of `known`.
    pub fn missing_part(owner: &str, part: &str, known: &[&str]) -> Self {
        Fact::MissingPart {
            owner: owner.to_string(),
            part: part.to_string(),
            suggestion: crate::parser::did_you_mean(part, known).map(str::to_string),
        }
    }

    pub fn unknown_module(name: &str) -> Self {
        Fact::UnknownModule {
            name: name.to_string(),
            available: vec!["math".to_string(), "random".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// 1-based source line, when known.
    pub line: Option<usize>,
    /// Byte range `[start, end)` of the offending source text, when known — lets
    /// the editor underline the exact span (a squiggle) rather than the whole
    /// line. `None` falls back to line-level highlighting.
    pub span: Option<(usize, usize)>,
    pub message: String,
    /// The error's family (see [`ErrorKind`]); `Syntax` unless a site says
    /// otherwise via [`CompileError::with_kind`].
    pub kind: ErrorKind,
    /// A stable key for THIS diagnostic (e.g. `type.str-plus-number`) when
    /// it has one — what ladders, the corpus, and the IDE key on. Legacy
    /// sites carry `None`; the type checker's promoted rules always set it.
    pub code: Option<&'static str>,
    /// What the mistake is about, where a site has recorded it.
    pub fact: Option<Box<Fact>>,
}

impl CompileError {
    pub fn at(line: usize, message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            span: None,
            message: message.into(),
            kind: ErrorKind::Syntax,
            code: None,
            fact: None,
        }
    }

    /// Like [`at`], but also carries the byte span of the offending text so the
    /// editor can underline exactly that range.
    pub fn at_span(line: usize, span: (usize, usize), message: impl Into<String>) -> Self {
        Self {
            line: Some(line),
            span: Some(span),
            message: message.into(),
            kind: ErrorKind::Syntax,
            code: None,
            fact: None,
        }
    }

    /// Like [`at`], but underlines the whole of `e` when its text is known.
    pub fn at_expr(e: &crate::ast::Expr, message: impl Into<String>) -> Self {
        let mut err = Self::at(e.line, message);
        if e.full_span != (0, 0) {
            err.span = Some(e.full_span);
        }
        err
    }

    /// An error with no specific location (rare — prefer `at`).
    pub fn general(message: impl Into<String>) -> Self {
        Self {
            line: None,
            span: None,
            message: message.into(),
            kind: ErrorKind::Syntax,
            code: None,
            fact: None,
        }
    }

    /// Classify this error (chainable): `CompileError::at(...).with_kind(Name)`.
    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }

    /// Record what the mistake is about (see the `fact` field).
    pub fn with_fact(mut self, fact: Fact) -> Self {
        self.fact = Some(Box::new(fact));
        self
    }

    /// The student-facing report: what happened, then what to try.
    pub fn kid_report(&self) -> String {
        let title = self.kind.title();
        let Some(fact) = &self.fact else {
            return format!("{title}: {}\n{self}", self.kind.explain());
        };
        let place = self.line.map_or(String::new(), |n| format!(" on line {n}"));
        format!(
            "{title}{place}: {}\nTry: {}",
            fact.explanation(),
            fact.next_step()
        )
    }

    /// Attach the diagnostic's stable key (see the `code` field).
    pub fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(n) => write!(f, "line {n}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for CompileError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kid_kinds_are_a_subset_of_the_full_set() {
        let families: std::collections::HashSet<_> =
            ErrorKind::ALL.iter().map(|k| k.family()).collect();
        assert_eq!(families.len(), 5);
        for k in ErrorKind::ALL {
            assert_eq!(k.family().family(), k.family(), "{k:?}");
            assert!(!k.explain().is_empty());
        }
    }

    #[test]
    fn titles_keep_the_exact_cpython_name_when_the_family_hides_it() {
        assert_eq!(ErrorKind::Name.title(), "Name Error");
        assert_eq!(ErrorKind::Attribute.title(), "Name Error (AttributeError)");
        assert_eq!(
            ErrorKind::Import.title(),
            "Name Error (ModuleNotFoundError)"
        );
        assert_eq!(ErrorKind::Index.title(), "Value Error (IndexError)");
        assert_eq!(ErrorKind::NotSupported.title(), "Not Supported Here");
    }

    #[test]
    fn errors_use_the_kind_cpython_would() {
        for (src, kind) in [
            ("import math\nprint(math.nope(1))\n", ErrorKind::Attribute),
            ("import turtle\n", ErrorKind::Import),
            ("for i in range(0, 5, 0):\n    print(i)\n", ErrorKind::Value),
            ("print(\"{} {}\".format(1))\n", ErrorKind::Index),
            ("x = 5\nx\n", ErrorKind::NotSupported),
            (
                "def f():\n    return 1\ndef f():\n    return 2\n",
                ErrorKind::NotSupported,
            ),
            ("break\n", ErrorKind::Syntax),
        ] {
            assert_eq!(error_for(src).kind, kind, "{src}");
        }
    }

    #[test]
    fn reports_use_the_student_s_own_names_and_say_what_to_try() {
        for (src, want) in [
            (
                "score = 1\nprint(scroe)\n",
                "Name Error on line 2: Python doesn't know `scroe` yet.\n\
                 Try: Did you mean `score`?",
            ),
            (
                "print(speed)\n",
                "Name Error on line 1: Python doesn't know `speed` yet.\n\
                 Try: Check the spelling, or set it above this line, like `speed = 0`.",
            ),
            (
                "def turn_left():\n    return 1\nturn_lft()\n",
                "Name Error on line 3: Python doesn't know `turn_lft` yet.\n\
                 Try: Did you mean `turn_left`?",
            ),
            (
                "wiggle()\n",
                "Name Error on line 1: Python doesn't know `wiggle` yet.\n\
                 Try: Check the spelling, or write `def wiggle():` above this line.",
            ),
            (
                "import math\nprint(math.sqr(4))\n",
                "Name Error (AttributeError) on line 2: `math` doesn't have a part called `sqr`.\n\
                 Try: Did you mean `math.sqrt`?",
            ),
            (
                "import turtle\n",
                "Name Error (ModuleNotFoundError) on line 1: There's no module called `turtle` here.\n\
                 Try: You can use: math, random.",
            ),
            (
                "def drive(speed):\n    return speed\nprint(drive())\n",
                "Type Error on line 3: `drive()` needs a value for `speed`, but this call doesn't give one.\n\
                 Try: Put a value inside the parentheses of `drive()`.",
            ),
            (
                "def drive(speed):\n    return speed\nprint(drive(1, 2))\n",
                "Type Error on line 3: `drive()` takes 1 value, but this call gives more.\n\
                 Try: Take out the extra values, or add parameters to `def drive`.",
            ),
        ] {
            assert_eq!(error_for(src).kid_report(), want, "{src}");
        }
    }

    #[test]
    fn errors_without_a_fact_fall_back_to_the_kind_s_sentence() {
        let report = error_for("break\n").kid_report();
        assert!(
            report.starts_with("Syntax Error: Python couldn't read"),
            "{report}"
        );
        assert!(
            report.contains("'break' can only be used inside a loop"),
            "{report}"
        );
    }

    fn error_for(src: &str) -> CompileError {
        crate::try_compile(src).expect_err(src)
    }

    #[test]
    fn name_and_type_mistakes_are_not_labelled_syntax_errors() {
        for (src, kind) in [
            ("foo(1)\n", ErrorKind::Name),
            ("class B(Missing):\n    pass\n", ErrorKind::Name),
            ("def f(a):\n    return a\nprint(f())\n", ErrorKind::Type),
            ("def f(a):\n    return a\nprint(f(1, 2))\n", ErrorKind::Type),
            ("print(\"a\" * \"b\")\n", ErrorKind::Type),
            ("print(\"a\" - 1)\n", ErrorKind::Type),
            ("import math\nprint(math.pow(1))\n", ErrorKind::Type),
        ] {
            assert_eq!(error_for(src).kind, kind, "{src}");
        }
    }

    #[test]
    fn a_mistake_in_an_expression_underlines_that_expression() {
        let src = "def f(a):\n    return a\nx = 1 + f()\n";
        let e = error_for(src);
        let (a, b) = e.span.expect("a span");
        assert_eq!(&src[a..b], "f()", "{}", e.message);
    }

    #[test]
    fn kinds_headline_in_pythons_vocabulary() {
        // The label a student learns must transfer to real CPython.
        assert_eq!(ErrorKind::Syntax.headline(), "Syntax Error");
        assert_eq!(ErrorKind::Name.headline(), "Name Error");
        assert_eq!(ErrorKind::Type.headline(), "Type Error");
        // Unclassified errors default to Syntax; with_kind reclassifies.
        assert_eq!(CompileError::at(1, "x").kind, ErrorKind::Syntax);
        assert_eq!(
            CompileError::at(1, "x").with_kind(ErrorKind::Name).kind,
            ErrorKind::Name
        );
    }
}
