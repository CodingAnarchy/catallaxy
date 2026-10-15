n = 100000
xs = []
for i in range(n):
    xs.append(i)
for i in range(10000):
    xs.extend([i, i, i])
for i in range(20000):
    xs.insert(0, i)
for i in range(1000):
    xs.pop(len(xs) - 1)
for i in range(1000):
    del xs[len(xs) - 1:]
total = 0
while len(xs) > 0:
    total += xs.pop(0)
print(total)
