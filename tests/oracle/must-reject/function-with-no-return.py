# DECIDED (2026-08-23): every path must return when any path does — total
# functions from day one. The error is the lesson: "what should this give
# back when the number is small?" Python's silent None on the missing path
# is the single most common source of real bugs in real code, and a
# twelve-year-old can be told to answer the question instead.
#
# The road not taken — int-or-None as a type — stays open for a later
# Optional tier; RPython's representation-derived rule (None mixes with
# pointer-shaped values, never with unboxed scalars) is the constraint it
# would have to respect on the Pico. The prior-art notes are kept below.
#
# WANTED: the error at the `def`, naming the line that returns a value and
# asking what the other path should give back.
#
# PRIOR ART (see PRIOR-ART-TYPES.md): mypy errors by default ("Missing return
# statement"); Mojo and Codon want an explicit Optional[T]. RPython gives the
# most useful answer because it is derived from REPRESENTATION, not taste:
# None may mix with "wrapped objects, class instances, lists, dicts, strings,
# etc. but NOT with int, floats or tuples" — nullable works where there is a
# pointer to make null, and does not where the value is unboxed. The
# "require every path to return" option had NO PRECEDENT among these
# projects — none of them was making a teaching argument — and is now ours.



def label(n: int) -> str:
    if n > 5:
        return "big"


print(label(2))
