# The string-length spelling that CROSSES: Mojo refuses len(<str>), but the
# hand-rolled counter — the week-three exercise — transfers unchanged for
# everyday text. Precision (probed against Mojo 1.0.0, not assumed): Python
# iterates CODE POINTS, Mojo iterates GRAPHEME CLUSTERS (what a person
# sees), and the two agree exactly when every grapheme is one code point —
# composed accents like this é, single emoji — i.e. keyboard-typed kid
# text. A decomposed accent or ZWJ emoji splits them (Python 2/3, Mojo 1).
# The accented case below also pins that neither side counts bytes (6).
def length(word: str) -> int:
    n = 0
    for c in word:
        n = n + 1
    return n

print(length("cafe"))
print(length("héllo"))
