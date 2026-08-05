# Fields get their types from what `__init__` assigns, and methods can use
# them without restating anything.
#
# Classes are the most recently landed part of the subset, so they are the
# most likely to be forgotten by a type checker written against the older
# parts of the language.


class Dog:
    def __init__(self, name: str, age: int):
        self.name = name
        self.age = age

    def describe(self) -> str:
        return self.name


d = Dog("Rex", 3)
print(d.describe())
print(d.age + 1)
