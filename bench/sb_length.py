parts = []
n = 0
total = 0
for i in range(200000):
    s = f"é{i % 10}"
    parts.append(s)
    n += len(s)
    total += n
print(total)
