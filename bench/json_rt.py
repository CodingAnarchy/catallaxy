import json
import sys
import time

records = []
for i in range(200000):
    records.append({
        "id": i,
        "name": f"record number {i}",
        "score": i * 0.25,
        "active": i % 2 == 0,
        "tags": [f"t{i % 7}", f"group-{i % 13}"],
        "note": "line\nbreak \"quoted\" é",
        "big": 12345678901234567890123 + i,
    })
t = time.monotonic()
text = json.dumps(records, separators=(",", ":"), sort_keys=True, ensure_ascii=False)
t1 = time.monotonic()
doc = json.loads(text)
t2 = time.monotonic()
for _ in range(3):
    doc = json.loads(json.dumps(doc, separators=(",", ":"), sort_keys=True, ensure_ascii=False))
t3 = time.monotonic()
print(f"stringify {round((t1 - t) * 1000)} ms, parse {round((t2 - t1) * 1000)} ms, "
      f"3 round trips {round((t3 - t2) * 1000)} ms", file=sys.stderr)
print(len(text))
print(len(json.dumps(doc, separators=(",", ":"), sort_keys=True, ensure_ascii=False)) == len(text))
