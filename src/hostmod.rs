//! Host modules: libraries a program imports whose functions the HOST
//! provides — the browser simulator, the step debugger, the native test
//! shims, or a robot's firmware. `robot` is the first (docs/ROBOT.md).
//!
//! One table drives every surface, so a function added here is known to all
//! of them at once: the WASM import name and signature, the native symbol,
//! the debugger's stand-in value, and the values the non-board hosts return.
//! Those stand-ins are what the browser shows before a simulator is attached
//! and what every test host returns, so all hosts print the same thing for
//! the same program. A board measures; everything else agrees on these.

/// A value crossing the host boundary. Ints and bools cross as `i32`, floats
/// as `f64`; `None` means the call returns nothing. A `Handler` parameter is
/// the name of a no-argument function the host calls later: it crosses as
/// the function's dispatch id, the same id `every` hands over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostTy {
    Int,
    Float,
    Bool,
    None,
    Handler,
}

impl HostTy {
    /// The WASM value type, or `""` for no value.
    pub fn wasm(self) -> &'static str {
        match self {
            HostTy::Int | HostTy::Bool | HostTy::Handler => "i32",
            HostTy::Float => "f64",
            HostTy::None => "",
        }
    }

    /// The LLVM IR type at the native host boundary.
    pub fn llvm(self) -> &'static str {
        match self {
            HostTy::Int | HostTy::Bool | HostTy::Handler => "i32",
            HostTy::Float => "double",
            HostTy::None => "void",
        }
    }

    /// The C type a native host implements the function with.
    pub fn c(self) -> &'static str {
        match self {
            HostTy::Int | HostTy::Bool | HostTy::Handler => "int",
            HostTy::Float => "double",
            HostTy::None => "void",
        }
    }
}

/// One function a host module provides.
#[derive(Debug)]
pub struct HostFn {
    pub name: &'static str,
    pub params: &'static [(&'static str, HostTy)],
    pub ret: HostTy,
    /// What a host without the hardware returns. Ignored for `HostTy::None`.
    pub stand_in: f64,
}

/// A named number a host module exports: a threshold such as
/// `robot.FLIPPED_DEG` (a float) or a state such as `robot.FLIPPED` (an int).
#[derive(Debug)]
pub struct HostConst {
    pub name: &'static str,
    pub value: f64,
    /// `Int` or `Float`: the Python type the constant has.
    pub ty: HostTy,
}

const fn int_const(name: &'static str, value: i32) -> HostConst {
    HostConst {
        name,
        value: value as f64,
        ty: HostTy::Int,
    }
}

const fn float_const(name: &'static str, value: f64) -> HostConst {
    HostConst {
        name,
        value,
        ty: HostTy::Float,
    }
}

#[derive(Debug)]
pub struct HostModule {
    pub name: &'static str,
    pub fns: &'static [HostFn],
    pub consts: &'static [HostConst],
}

const fn f(
    name: &'static str,
    params: &'static [(&'static str, HostTy)],
    ret: HostTy,
    stand_in: f64,
) -> HostFn {
    HostFn {
        name,
        params,
        ret,
        stand_in,
    }
}

/// The robot module: plain calls, constants, and two subscriptions.
///
/// A sensor a game cares about has a measurement and a state. `tilt()` is the
/// measurement, in degrees; `tilt_state()` is the state the host last
/// classified it as: `LEVEL`, `TILTED` or `FLIPPED`. An `on_` handler runs
/// whenever its state changes and reads the new state itself, so one rule
/// covers every event. States are ints compared with `==`; measurements are
/// floats compared with `>=` against the `_DEG` thresholds.
///
/// Stand-ins describe a robot alone on a flat floor: nothing in range, no
/// ring edge, level, free, so a host without hardware never fires a handler.
pub const ROBOT: HostModule = HostModule {
    name: "robot",
    fns: &[
        f(
            "set_motor",
            &[("left", HostTy::Int), ("right", HostTy::Int)],
            HostTy::None,
            0.0,
        ),
        f("stop", &[], HostTy::None, 0.0),
        f(
            "read_distance",
            &[("sensor", HostTy::Int)],
            HostTy::Int,
            2000.0,
        ),
        f("read_line", &[("sensor", HostTy::Int)], HostTy::Bool, 0.0),
        f("tilt", &[], HostTy::Float, 0.0),
        f("tilt_state", &[], HostTy::Int, 0.0),
        f("tag_state", &[], HostTy::Int, 0.0),
        f(
            "on_tilt",
            &[("handler", HostTy::Handler)],
            HostTy::None,
            0.0,
        ),
        f("on_tag", &[("handler", HostTy::Handler)], HostTy::None, 0.0),
    ],
    consts: &[
        float_const("TILTED_DEG", 30.0),
        float_const("FLIPPED_DEG", 150.0),
        int_const("LEVEL", 0),
        int_const("TILTED", 1),
        int_const("FLIPPED", 2),
        int_const("FREE", 0),
        int_const("TAGGED", 1),
        int_const("IMMUNE", 2),
    ],
};

/// How far back past a threshold a robot must come before its tilt state
/// steps back: a reading wobbling around 150 degrees is one flip, not a
/// handler call every 10 ms.
pub const TILT_HYSTERESIS_DEG: f64 = 10.0;

/// Every host module, in the order error messages list them.
pub const HOST_MODULES: &[&HostModule] = &[&ROBOT];

/// The host module named `name`, if there is one.
pub fn module(name: &str) -> Option<&'static HostModule> {
    HOST_MODULES.iter().copied().find(|m| m.name == name)
}

impl HostModule {
    pub fn func(&self, name: &str) -> Option<&'static HostFn> {
        self.fns.iter().find(|f| f.name == name)
    }

    /// Whether `func` takes a handler: a subscription such as `on_tilt`.
    pub fn takes_handler(&self, func: &str) -> bool {
        self.func(func)
            .is_some_and(|f| f.params.iter().any(|(_, t)| *t == HostTy::Handler))
    }

    pub fn constant(&self, name: &str) -> Option<f64> {
        self.constant_def(name).map(|c| c.value)
    }

    pub fn constant_def(&self, name: &str) -> Option<&'static HostConst> {
        self.consts.iter().find(|c| c.name == name)
    }

    /// Every name a program can use after `module.`: functions, then
    /// constants. Feeds "did you mean" suggestions.
    pub fn names(&self) -> Vec<&'static str> {
        self.fns
            .iter()
            .map(|f| f.name)
            .chain(self.consts.iter().map(|c| c.name))
            .collect()
    }
}

impl HostFn {
    /// The WASM import name under `env`: `robot_set_motor`.
    pub fn import_name(&self, module: &str) -> String {
        format!("{module}_{}", self.name)
    }

    /// The native symbol a host defines: `p2w_robot_set_motor`.
    pub fn native_symbol(&self, module: &str) -> String {
        format!("p2w_{module}_{}", self.name)
    }

    /// The WASM import declaration for this function.
    pub fn wasm_import(&self, module: &str) -> String {
        let name = self.import_name(module);
        let params: String = self
            .params
            .iter()
            .map(|(_, t)| format!(" (param {})", t.wasm()))
            .collect();
        let result = match self.ret {
            HostTy::None => String::new(),
            t => format!(" (result {})", t.wasm()),
        };
        format!(r#"(import "env" "{name}" (func ${name}{params}{result}))"#)
    }

    /// The LLVM declaration of the native symbol.
    pub fn llvm_declare(&self, module: &str) -> String {
        let params: Vec<&str> = self.params.iter().map(|(_, t)| t.llvm()).collect();
        format!(
            "declare {} @{}({})",
            self.ret.llvm(),
            self.native_symbol(module),
            params.join(", ")
        )
    }

    /// A C definition returning the stand-in, for hosts without hardware.
    pub fn c_stand_in(&self, module: &str) -> String {
        let params: Vec<String> = self
            .params
            .iter()
            .map(|(n, t)| format!("{} {n}", t.c()))
            .collect();
        let params = if params.is_empty() {
            "void".to_string()
        } else {
            params.join(", ")
        };
        let unused: String = self
            .params
            .iter()
            .map(|(n, _)| format!(" (void){n};"))
            .collect();
        let body = match self.ret {
            HostTy::None => String::new(),
            HostTy::Float => format!(" return {:?};", self.stand_in),
            HostTy::Int | HostTy::Bool => format!(" return {};", self.stand_in as i64),
            HostTy::Handler => unreachable!("a handler is a parameter, never a result"),
        };
        format!(
            "{} {}({params}) {{{unused}{body} }}",
            self.ret.c(),
            self.native_symbol(module)
        )
    }
}

/// The error for a subscription given something other than a no-argument
/// function, worded the same on every backend. `got` is the name the program
/// passed, or `None` when it passed an expression that isn't a name.
pub fn handler_error(module: &str, func: &str, got: Option<&str>) -> String {
    match got {
        Some(name) => {
            format!("'{name}' must be a function with no arguments to run with {module}.{func}()")
        }
        None => {
            let example = format!("{}_changed", func.strip_prefix("on_").unwrap_or(func));
            format!(
                "{module}.{func}() needs the name of a function, like {module}.{func}({example})"
            )
        }
    }
}

/// C stand-ins for every host function, for native hosts without hardware.
pub fn c_stand_ins() -> String {
    let mut out = String::new();
    for m in HOST_MODULES {
        for f in m.fns {
            out.push_str(&f.c_stand_in(m.name));
            out.push('\n');
        }
    }
    out
}

/// The module names, as an error message lists them: "math, random and robot".
pub fn available_modules() -> String {
    let mut all = vec!["math", "random"];
    all.extend(HOST_MODULES.iter().map(|m| m.name));
    match all.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        _ => all.join(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_render_for_each_surface() {
        let set_motor = ROBOT.func("set_motor").unwrap();
        assert_eq!(
            set_motor.wasm_import("robot"),
            r#"(import "env" "robot_set_motor" (func $robot_set_motor (param i32) (param i32)))"#
        );
        assert_eq!(
            ROBOT.func("tilt").unwrap().llvm_declare("robot"),
            "declare double @p2w_robot_tilt()"
        );
        assert_eq!(
            ROBOT.func("read_distance").unwrap().c_stand_in("robot"),
            "int p2w_robot_read_distance(int sensor) { (void)sensor; return 2000; }"
        );
        assert_eq!(
            ROBOT.func("tilt").unwrap().c_stand_in("robot"),
            "double p2w_robot_tilt(void) { return 0.0; }"
        );
    }

    #[test]
    fn names_cover_functions_and_constants() {
        let names = ROBOT.names();
        assert!(names.contains(&"tilt") && names.contains(&"FLIPPED"));
        assert_eq!(ROBOT.constant("FLIPPED_DEG"), Some(150.0));
        assert_eq!(ROBOT.constant_def("FLIPPED").unwrap().ty, HostTy::Int);
        assert!(ROBOT.func("FLIPPED").is_none());
    }

    #[test]
    fn available_modules_reads_as_a_list() {
        assert_eq!(available_modules(), "math, random and robot");
    }

    /// The native shims and the board's boot code are hand-written C, so a
    /// row added to the table must show up there too or every native program
    /// using it fails to link.
    #[test]
    fn native_hosts_define_every_function() {
        let root = env!("CARGO_MANIFEST_DIR");
        for file in [
            "tools/native_run.sh",
            "tests/backend_diff.rs",
            "device/boot.c",
        ] {
            let text = std::fs::read_to_string(format!("{root}/{file}")).unwrap();
            for m in HOST_MODULES {
                for f in m.fns {
                    let sym = f.native_symbol(m.name);
                    assert!(text.contains(&sym), "{file} does not define {sym}");
                }
            }
            // Not a host module, but every native host needs it just the same.
            if file == "device/boot.c" {
                // The board classifies tilt with the table's numbers.
                for (name, value) in [
                    ("TILTED_DEG", ROBOT.constant("TILTED_DEG").unwrap()),
                    ("FLIPPED_DEG", ROBOT.constant("FLIPPED_DEG").unwrap()),
                    ("HYSTERESIS_DEG", TILT_HYSTERESIS_DEG),
                ] {
                    assert!(
                        text.contains(&format!("#define {name:<15} {value:?}")),
                        "boot.c's {name} is not {value:?}"
                    );
                }
                for (name, value) in [("LEVEL", 0), ("TILTED", 1), ("FLIPPED", 2)] {
                    assert_eq!(ROBOT.constant(name), Some(value as f64), "{name}");
                }
            }
            assert!(
                text.contains("p2w_host_every"),
                "{file} does not define p2w_host_every"
            );
        }
    }
}
