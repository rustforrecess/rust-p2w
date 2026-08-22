# p2w_prelude.mojo — the Mojo side of the p2w bridge (docs/MOJO_BRIDGE.md).
#
# A p2w program that passes `p2w check --profile mojo` uses Python's lowercase
# type names in its annotations — real Python, CPython-runnable, the p2w
# invariant. Mojo spells the same types with capitals. These compile-time
# bindings close that gap so the program TEXT compiles unchanged as Mojo:
# the shim lives here, on Mojo's side, never in the student's program.
#
# STATUS: verified against Mojo 1.0.0 (ed45d567) by the mojo-bridge CI job —
# tests/mojo_bridge/ programs run under real Mojo and match CPython's output
# byte-for-byte. (`comptime`, not `alias`: 1.0 deprecated the old keyword.)

comptime int = Int
comptime float = Float64
comptime str = String
comptime bool = Bool

# Deliberately NOT mapped — the profile flags programs that need these:
#   * len() of a string: a hard ERROR in Mojo 1.0 (UTF-8 makes one length
#     ambiguous; Python's len(str) means code points). No clean shim exists.
#   * Python's `random` module (Mojo's random has different names/semantics)
#   * f-strings (no Mojo equivalent; the profile suggests `+` and `str(...)`)
#   * classes (Mojo's Phase 3 hasn't begun — there is nothing to map to)
