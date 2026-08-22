# The negative control: a class program is valid p2w — the canonical
# curriculum shape — and the profile must REFUSE it for Mojo (their Phase 3
# hasn't begun). If this ever becomes "ready", the profile broke.
class Dog:
    def __init__(self, name):
        self.name = name

    def speak(self):
        return self.name + " barks"

d = Dog("Rex")
print(d.speak())
