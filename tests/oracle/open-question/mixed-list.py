# DECIDE: what is the type of a list holding more than one kind of thing?
#
# Python is happy. A homogeneous `list[T]` is what makes unboxed, specialised
# lists possible — and those are what the Pico target needs, so this is not
# only a type question, it is a memory-layout question.
#
# Options: infer a union and box the elements; reject with "a list holds one
# kind of thing"; or accept but fall back to a boxed representation and lose
# the optimisation quietly.
#
# The third is the dangerous one: silent performance cliffs are exactly what a
# glass-box system should not have.
#
# PRIOR ART (see PRIOR-ART-TYPES.md): unanimous HOMOGENEOUS among everything
# that compiles. Only mypy allows it, by joining to list[object] — and mypy has
# no memory layout to satisfy.
#
# Codon supplies the answer worth giving a student: TUPLES stay heterogeneous,
# because their length and element layout are statically known. "A list holds
# many of one thing; a tuple holds a few different things" is true about the
# machine and teachable at the same time, which is rare.

things = [1, "two", 3.0]
print(things[0])
