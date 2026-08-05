# Annotations that match what inference would have worked out anyway.
#
# The checker must agree with itself: writing down the true type can never
# turn a working program into a broken one.

count: int = 0
name: str = "Ada"
ratio: float = 0.5

print(name)
print(count + 1)
print(ratio * 2.0)
