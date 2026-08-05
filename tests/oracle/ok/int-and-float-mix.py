# Python promotes int to float in mixed arithmetic. `1 + 2.5` is 3.5, not an
# error, and `4 / 2` is a float even though both operands are ints.
#
# A checker that treats int and float as unrelated will reject perfectly good
# arithmetic. Whatever the numeric story ends up being, these must compile.

a = 1 + 2.5
b = 3 * 1.5
c = 7 / 2

print(a)
print(b)
print(c)
