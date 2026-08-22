# p2w_prelude.mojo — the Mojo side of the p2w bridge (docs/MOJO_BRIDGE.md).
#
# A p2w program that passes `p2w check --profile mojo` uses Python's lowercase
# type names in its annotations — real Python, CPython-runnable, the p2w
# invariant. Mojo spells the same types with capitals. These compile-time
# aliases close that gap so the program TEXT compiles unchanged as Mojo:
# the shim lives here, on Mojo's side, never in the student's program.
#
# STATUS: v1, differential verification pending. The planned job compiles the
# profile-clean oracle programs with the real Mojo compiler (Apache 2.0 since
# 2026-08-18) and diffs their output; until it lands, this file is the spec
# of intent, exercised by hand.

alias int = Int
alias float = Float64
alias str = String
alias bool = Bool

# Deliberately NOT mapped in v1 — the profile flags programs that need these:
#   * Python's `random` module (Mojo's random has different names/semantics)
#   * f-strings (no Mojo equivalent; the profile suggests `+` and `str(...)`)
#   * classes (Mojo's Phase 3 hasn't begun — there is nothing to map to)
