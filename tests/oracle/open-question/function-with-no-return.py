# DECIDE: what does a function that sometimes returns nothing have as a type?
#
# In Python the else-path returns None, so the result is int-or-None. This is
# the doorway to optional types, and it is the single most common source of
# real bugs in real code.
#
# It also decides whether `None` exists as a type students can be told about.
# There is a genuine pedagogical case for going the other way: require every
# path to return, and make the error "what should this give back when the
# number is small?" That teaches total functions from day one, which most
# adult languages wish they had.
#
# Either answer is defensible. Not answering is not.
#
# PRIOR ART (see PRIOR-ART-TYPES.md): mypy errors by default ("Missing return
# statement"); Mojo and Codon want an explicit Optional[T]. RPython gives the
# most useful answer because it is derived from REPRESENTATION, not taste:
# None may mix with "wrapped objects, class instances, lists, dicts, strings,
# etc. but NOT with int, floats or tuples" — nullable works where there is a
# pointer to make null, and does not where the value is unboxed.
#
# That is our Pico constraint, reached independently by someone who shipped it.
# If we get unboxed scalars, Optional[int] costs a tag and Optional[Dog] costs
# nothing, and that difference exists whether or not we explain it.
#
# The "require every path to return" option has NO PRECEDENT among these
# projects — and nothing contradicting it either. None of them was making a
# teaching argument.


def label(n: int) -> str:
    if n > 5:
        return "big"


print(label(2))
