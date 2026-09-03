# An early return followed by a genuine forever loop (a robot idle shape).
# The loop diverges, so "can finish without a value" is false; the gate
# must accept. Companion false positive to retry-loop-returns-inside.py.
def wait(n: int) -> int:
    if n > 0:
        return n
    while True:
        pass

print(wait(5))
