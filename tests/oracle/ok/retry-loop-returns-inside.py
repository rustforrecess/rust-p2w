# The retry loop: `while True:` with the return inside. Control cannot
# fall off the end (the loop has no break), so the function never finishes
# without a value — the missing-return gate must accept it. This was a live
# false positive found 2026-08-31.
def pick() -> int:
    while True:
        n = 3
        if n > 2:
            return n

print(pick())
