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


def label(n: int) -> str:
    if n > 5:
        return "big"


print(label(2))
