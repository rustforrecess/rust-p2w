# The string-length spelling that CROSSES: Mojo refuses len(<str>) (UTF-8
# ambiguity), but `for c in word` iterates code points in BOTH languages —
# so the hand-rolled counter, the week-three exercise, transfers unchanged
# and agrees with CPython's len() semantics. (Verified against Mojo 1.0.0;
# the accented case pins code points, not bytes — bytes would say 6.)
def length(word: str) -> int:
    n = 0
    for c in word:
        n = n + 1
    return n

print(length("cafe"))
print(length("héllo"))
