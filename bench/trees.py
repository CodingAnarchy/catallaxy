class Tree:
    __slots__=("left","right")
    def __init__(s,l,r): s.left=l; s.right=r
def make(d): return Tree(None,None) if d==0 else Tree(make(d-1),make(d-1))
def check(t): return 1 if t.left is None else 1+check(t.left)+check(t.right)
total=0
for i in range(20): total+=check(make(16))
print(total)
