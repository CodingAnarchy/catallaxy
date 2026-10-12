counts={}
words=["alpha","beta","gamma","delta","epsilon","zeta","eta","theta"]
for i in range(2000000):
    w=words[i%8]; counts[w]=counts.get(w,0)+1
print(len("".join(f"{k}={v};" for k,v in counts.items())))
