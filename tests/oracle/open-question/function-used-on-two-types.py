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


def first(items):
    return items[0]


print(first([1, 2, 3]))
print(first(["a", "b"]))
