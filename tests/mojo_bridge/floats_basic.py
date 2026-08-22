# Floats across the bridge. Values are chosen to be exactly representable
# (halves and quarters) so the diff tests SEMANTICS, not the two languages'
# float-formatting corners — those get their own case once the bridge is
# proven on the easy ground.
def average(a: float, b: float) -> float:
    return (a + b) / 2.0

h = 2.5
print(h + 1.5)
print(7 / 2)
print(10.0 / 4.0)
print(average(1.0, 4.0))
