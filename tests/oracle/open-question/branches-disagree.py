# DECIDE: the same question as reassignment, but arriving through control flow
# — which is where it actually bites.
#
# Nobody writes `x = 1` then `x = "one"` on purpose. People write this all the
# time: one branch produces a number, the other a message. If reassignment is
# forbidden, this must be rejected too, and the error has to explain the
# BRANCH, not just the line.
#
# Whatever is decided here must match the reassignment answer. Two different
# answers for the same underlying question is how a language becomes folklore.

n = 7
if n > 5:
    result = n * 2
else:
    result = "too small"

print(result)
