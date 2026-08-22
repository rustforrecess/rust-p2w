# Strings across the bridge: concatenation, len, str() of an int (which the
# prelude's `alias str = String` turns into Mojo's String constructor).
def shout(word: str) -> str:
    return word + "!"

s = "abc" + "def"
print(s)
print(len(s))
print(shout("hey"))
print(str(42) + "x")
