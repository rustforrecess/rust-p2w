# Declared `-> int`, returns a string.
#
# WANTED: the error at the `return`, quoting the signature — the promise is on
# one line and the break is on another, and both belong in the message.


def double(n: int) -> int:
    return "twice " + str(n)


print(double(4))
