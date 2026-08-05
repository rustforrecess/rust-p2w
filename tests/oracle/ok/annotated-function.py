# The house style: annotated parameters and return type.
#
# This is the shape we teach once students are ready for it, so it has to be
# the shape the checker is happiest with.


def area(width: int, height: int) -> int:
    return width * height


print(area(3, 4))
