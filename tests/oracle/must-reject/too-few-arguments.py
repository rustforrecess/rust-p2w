# Arity, not types — but it lives here because it is the same kind of promise
# and a student cannot tell the difference.
#
# WANTED: "area needs 2 things, you gave 1" plus which one is missing by name.


def area(width: int, height: int) -> int:
    return width * height


print(area(3))
