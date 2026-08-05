# The single most common shape in beginner code: start at zero, add in a loop.
#
# Inference has to survive a variable being assigned in two places (once
# before the loop, once inside it) and conclude the obvious thing. If this
# needs an annotation, the checker is not usable in a classroom.

total = 0
for i in range(5):
    total = total + i
print(total)
