# `+` works on strings, so `-` looks like it should too. It does not.
#
# WANTED: a message that acknowledges the reasonable expectation — `+` joins
# strings, but there is no meaning for taking one away — rather than a bare
# "unsupported operand".

full = "hello world"
short = full - "world"
print(short)
