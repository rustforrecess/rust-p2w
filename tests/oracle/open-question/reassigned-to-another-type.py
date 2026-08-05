# DECIDE: may a name change type?
#
# Python allows this. Most static checkers do not, or need union types to
# express it. It is the first fork in the road and everything downstream
# depends on it.
#
# Arguments for allowing: it is real Python, and forbidding it means a program
# that runs in CPython does not compile here.
# Arguments for forbidding: it is nearly always a mistake in beginner code,
# rejecting it produces an excellent error, and allowing it means every use of
# a variable must consider every branch that could have reached it — which is
# what makes type errors report the wrong line.
#
# NOTE the third option: allow it in general but WARN, as a lint with a fix
# ladder rather than an error. That is the shape the rest of the toolchain
# already uses for "legal but probably not what you meant".

answer = 42
answer = "forty-two"
print(answer)
