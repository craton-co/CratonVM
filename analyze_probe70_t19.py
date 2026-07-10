from collections import Counter, defaultdict
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
lines = path.read_text(errors="replace").splitlines()

dump_re = re.compile(r'^(?P<prefix>\[[^]]+\] )?--- T19\.H1 stack dump: tid=(?P<tid>\d+) name="(?P<name>[^"]*)" frames=(?P<frames>\d+) ---')
frame_re = re.compile(r'^(?P<prefix>\[[^]]+\] )?tid=(?P<tid>\d+) depth=(?P<depth>\d+) class=(?P<class>\S+) method=(?P<method>\S+) desc=(?P<desc>\S+) pc=(?P<pc>\S+)')

records = []
i = 0
while i < len(lines):
    m = dump_re.match(lines[i])
    if not m:
        i += 1
        continue
    prefix = (m.group("prefix") or "").strip()
    tid = m.group("tid")
    name = m.group("name")
    frames = []
    i += 1
    while i < len(lines) and "T19.H1 end dump" not in lines[i]:
        fm = frame_re.match(lines[i])
        if fm:
            frames.append((fm.group("class"), fm.group("method"), fm.group("desc"), fm.group("pc")))
        i += 1
    records.append((prefix, tid, name, tuple(frames)))
    i += 1

print(f"records={len(records)}")

top_counts = Counter()
by_prefix = defaultdict(Counter)
examples = {}
for prefix, tid, name, frames in records:
    top = frames[0] if frames else ("<no-frame>", "", "", "")
    key = f"{top[0]}.{top[1]} {top[2]}"
    top_counts[key] += 1
    by_prefix[prefix or "<none>"][key] += 1
    examples.setdefault(key, (prefix, tid, name, frames[:8]))

print("\nTop frames:")
for key, count in top_counts.most_common(30):
    print(f"{count:5d} {key}")

print("\nBy prefix:")
for prefix, counter in sorted(by_prefix.items()):
    print(f"[{prefix}]")
    for key, count in counter.most_common(12):
        print(f"  {count:5d} {key}")

print("\nExamples:")
for key, count in top_counts.most_common(12):
    prefix, tid, name, frames = examples[key]
    print(f"\n{count}x {key} example prefix={prefix or '<none>'} tid={tid} name={name}")
    for depth, frame in enumerate(frames):
        print(f"  {depth}: {frame[0]}.{frame[1]} {frame[2]} pc={frame[3]}")
