# The function said what it wanted. The call site ignored it.
#
# WANTED: the error at the CALL, not inside the function body. The function is
# correct; the caller is wrong, and a message pointing inside `area` would send
# the student to edit working code.


def area(width: int, height: int) -> int:
    return width * height


print(area("3", 4))
