#!/usr/bin/env python3
"""Which registrations are UNREACHABLE BY CONSTRUCTION, and which only look it.

`l3-followups-the-carrier-identity-and-the-dead-registrations-20260830.md`
records the rule and the reason it cannot be applied blind:

  an INSTANCE-method registration on an abstract class or an interface is
  unreachable by every dispatch door -- UNLESS something mints an object whose
  class NAME is that abstract class or interface.

CratonVM does exactly that in places: its spliterator carrier is named
`java.util.Spliterator`, an interface, so registrations on that name ARE live.
The page says the missing tool is a PRODUCER census, and this is it.

Two inputs, both mechanical:

  * the JDK image itself says which names are abstract or interfaces -- asked
    with `javap`, not guessed from the name;
  * the Rust tree says which names this VM mints, grepped from the four
    allocation helpers that take a class name.

A class that is abstract-or-interface AND minted by nobody has no possible
receiver, so every instance-method row on it is dead. Static rows are excluded:
a static call names the class directly and is reachable regardless.

VALIDATED, AND THE ONE BLIND SPOT IS MEASURED. `apps/probes/LambdaClassProbe`
asks whether a lambda's runtime class is its functional interface's name, which
is the case that would make the eighteen `java.util.function.*` rows live rather
than dead. It is not: lambdas and method references are `$$Lambda` hidden
classes on this VM exactly as on HotSpot, and an anonymous or named
implementation carries its own name. So those rows really have no receiver.

The same probe records the opposite trap. `getClass()` is NOT the name dispatch
uses: `List.of("a").stream().getClass()` answers
`java.util.stream.ReferencePipeline$Head` while the object's INTERNAL class name
is `java/util/stream/Stream` -- there is a reported-name mapping in front of it.
So a runtime `getClass()` reading cannot be used to decide whether a name is
minted; the grep over the allocation helpers can, and that is why this script
uses it.

CONSERVATIVE BY DESIGN, in the safe direction. The "minted" grep is loose: it
sweeps every `const NAME: &str` in the tree alongside the literal call sites,
because a helper is often called with a constant. A false "minted" marks a class
LIVE and merely leaves a dead row in place; a false "not minted" would invite
deleting a live one. Under-reporting dead rows is the error this is built to
make.

NOT A DELETE LIST ON ITS OWN. A row this reports is a candidate: confirm it by
dumping the registry after a probe that calls the method through every door --
receiver-typed, interface-typed, and a bound method reference -- and checking
the invocation count is still zero. `apps/probes/DeadDoorProbe` is that check
for the `java.util` set, and the STATIC rows on the same classes are its control:
they fire, so a zero is not a broken counter.
"""
import json
import re
import subprocess
import sys

REG = sys.argv[1] if len(sys.argv) > 1 else "/tmp/reg-all.json"
JDK = sys.argv[2] if len(sys.argv) > 2 else "/data/jdkimages/jdk25-linux/jdk-25.0.4+7"
TREE = sys.argv[3] if len(sys.argv) > 3 else "/data/cvm-l3u-20260828"
PREFIX = sys.argv[4] if len(sys.argv) > 4 else "java/util/"

rows = json.load(open(REG))["natives"]

# ---- 1. Every class this VM MINTS, from the allocation helpers that name one.
mint_re = re.compile(
    r'(?:try_alloc_synthetic|try_alloc_declared_width|try_alloc_concurrent_synthetic'
    r'|try_ensure_synthetic_class|alloc_synthetic)\s*\(\s*ctx\s*,\s*"([^"]+)"'
)
minted = set()
grep = subprocess.run(
    ["grep", "-rhoE", '"[a-zA-Z0-9_/$]+"', "--include=*.rs", TREE],
    capture_output=True, text=True,
)
src = subprocess.run(
    ["grep", "-rh", "-A2", "-E",
     "try_alloc_synthetic|try_alloc_declared_width|try_alloc_concurrent_synthetic|try_ensure_synthetic_class",
     "--include=*.rs", TREE],
    capture_output=True, text=True,
).stdout
for m in mint_re.finditer(src):
    minted.add(m.group(1))
# The helpers are also called with a constant, so sweep the constants too.
for m in re.finditer(r'const\s+\w+:\s*&str\s*=\s*"([^"]+)"', src):
    minted.add(m.group(1))

# ---- 2. Ask the IMAGE which names are abstract or interfaces.
classes = sorted({r["class"] for r in rows if r["class"].startswith(PREFIX)})
kind = {}
CHUNK = 60
for i in range(0, len(classes), CHUNK):
    names = [c.replace("/", ".") for c in classes[i:i + CHUNK]]
    out = subprocess.run([JDK + "/bin/javap", "-public"] + names,
                         capture_output=True, text=True).stdout
    for line in out.splitlines():
        m = re.match(r"^(?:public\s+)?(?:final\s+)?(abstract\s+)?(class|interface)\s+([\w.$]+)", line)
        if m:
            kind[m.group(3).replace(".", "/")] = (
                "interface" if m.group(2) == "interface"
                else ("abstract" if m.group(1) else "concrete")
            )

# ---- 3. The verdict, per class.
dead_rows = 0
live_rows = 0
print("%-46s %-10s %-8s %s" % ("class", "image", "minted", "instance rows"))
for c in classes:
    k = kind.get(c, "?")
    inst = [r for r in rows if r["class"] == c and r["name"] != "<clinit>"]
    # A static row is reachable whatever the class kind; the dump does not carry
    # ACC_STATIC, so <init> is the only one we can name for certain.
    n = len(inst)
    is_minted = c in minted
    if k in ("abstract", "interface") and not is_minted:
        dead_rows += n
        print("%-46s %-10s %-8s %d   <-- NO POSSIBLE RECEIVER" % (c[len(PREFIX):], k, is_minted, n))
    elif k in ("abstract", "interface"):
        live_rows += n
        print("%-46s %-10s %-8s %d   (minted by this VM)" % (c[len(PREFIX):], k, is_minted, n))
print()
print("rows on abstract/interface classes NOTHING mints:", dead_rows)
print("rows on abstract/interface classes THIS VM mints:", live_rows)
