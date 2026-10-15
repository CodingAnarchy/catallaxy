from itertools import chain

xs = list(range(500000))
ys = [y for x in xs for y in (x, x)]
print(len(ys))
zs = list(chain.from_iterable((x, x + 1) for x in xs))
print(len(zs))
nested = [[x, -x] for x in xs]
flat = [y for n in nested for y in n]
print(len(flat))
flat = [x for x in flat if x % 3 != 0]
print(len(flat))
r = [y for i in range(1000) for y in list(range(1000))]
print(len(r))
