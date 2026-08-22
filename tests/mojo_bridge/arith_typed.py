# The intersection's core: typed defs, loops, conditionals, integer
# arithmetic. Double-quoted strings (Mojo's documented literal form).
def double(n: int) -> int:
    return n * 2

def add(a: int, b: int) -> int:
    return a + b

total = 0
for i in range(1, 6):
    total = total + i
print(total)
print(double(21))
print(add(double(3), 4))
x = 10
while x > 0:
    x = x - 3
print(x)
if total > 10:
    print("big")
else:
    print("small")
print(7 // 2)
print(7 % 2)
