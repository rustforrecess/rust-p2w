# DECIDE: how polymorphic is an unannotated function?
#
# `first` works on any list. Called on ints and then on strings, a checker
# without generalisation locks it to whichever came first and rejects the
# second call — with a baffling message pointing at a function that is fine.
#
# This is the let-polymorphism question, and it is the part of inference that
# unification alone does not give you. It is worth being explicit that this is
# a SEPARATE piece of work from the unifier, because it is the piece most
# likely to be assumed for free.
#
# The cheap answer is to require an annotation on any function used at more
# than one type. That is defensible for a teaching language — but it should be
# a decision, with the error message that makes it teachable, not an accident.
#
# PRIOR ART (see PRIOR-ART-TYPES.md): THIS IS THE QUESTION WHERE THEY ACTUALLY
# DISAGREE — four live strategies. Shed Skin duplicates per tuple of argument
# types automatically (Agesen's Cartesian Product Algorithm); RPython unifies
# all call sites into one signature and makes you opt in to duplication;
# Codon monomorphises; Mojo makes you write the parameters; SPy resolves it at
# compile time during redshift.
#
# Two things that narrow OUR choice:
#   - Whole-program analysis is normally rejected for breaking separate
#     compilation. We compile one program, in a browser, with no linking model.
#     The approach that is expensive for everyone else is cheap for us.
#   - Monomorphisation and the FBIP/unboxed-list plan are the SAME MECHANISM.
#     If list[i64] must be unboxed on the Pico, we are already specialising per
#     type — so this answer is partly forced by the memory model, and probably
#     should be decided alongside layout rather than before it.


def first(items):
    return items[0]


print(first([1, 2, 3]))
print(first(["a", "b"]))
