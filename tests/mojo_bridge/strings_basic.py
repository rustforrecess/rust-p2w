# Strings across the bridge: concatenation, str() of an int (which the
# prelude's `str = String` binding turns into Mojo's constructor), and an
# ANNOTATED first assignment — valid Python, and the spelling Mojo is moving
# toward (implicit declaration is deprecated in 1.0). len(s) is deliberately
# absent: Mojo makes it a hard error (UTF-8 ambiguity) and the profile
# flags it.
def shout(word: str) -> str:
    return word + "!"

s: str = "abc" + "def"
print(s)
print(shout("hey"))
print(str(42) + "x")
