from functools import cmp_to_key

xs = []
x = 12345
for i in range(1000000):
    x = (x * 1103515245 + 12345) % 2147483648
    xs.append(x % 1000000)
s = sorted(xs, key=cmp_to_key(lambda p, q: (q > p) - (q < p)))
print(s[0])
print(s[-1])
total = 0
for r in range(10):
    total += sum(xs)
print(total)
print(min(xs, key=lambda v: v % 1000))
print(max(xs, key=lambda v: v % 1000))
