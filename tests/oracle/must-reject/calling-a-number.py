# A name is reused: first a number, then called as if it were a function.
#
# Usually a typo or a forgotten earlier assignment. WANTED: the message should
# say where `total` became a number, since that is the line to look at.
#
# ALREADY REJECTED — but with the WRONG REASON. Today it says "unknown
# function 'total'", and `total` is not unknown at all: it is a number, bound
# one line up. A student reads that and starts hunting for a misspelling that
# does not exist. Fixing the message needs the checker to know what the name
# IS, which is precisely the thing that does not exist yet. Left here as a
# message-quality target, not a rejection target.

total = 5
print(total(3))
