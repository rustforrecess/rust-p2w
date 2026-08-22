# The second negative control: this program is valid Python AND would be
# valid Mojo — it compiles and runs on both — but the family emoji is one
# grapheme built from several code points, so Python's loop counts pieces
# while Mojo's counts what a person sees: the SAME program printing
# DIFFERENT numbers. The profile must refuse it outright; silent divergence
# is worse than an error.
def length(word: str) -> int:
    n = 0
    for c in word:
        n = n + 1
    return n

print(length("👨‍👩‍👧"))
