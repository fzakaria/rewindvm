# Prints from Greeter.greet a few calls deep, for CPython's py-bt.
class Greeter:
    def __init__(self, name):
        self.name = name
        self.counts = [1, 2, 3]

    def greet(self, who):
        self.counts.append(len(who))
        print("hello from python,", who, self.name, self.counts, flush=True)


def main():
    g = Greeter("python")
    for who in ["alice", "bob"]:
        g.greet(who)


main()
