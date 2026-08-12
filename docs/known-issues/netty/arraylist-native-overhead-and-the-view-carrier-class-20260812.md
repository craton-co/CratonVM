# Collection views are carrier-classed as `java.util.ArrayList`, which both misreports `getClass()` and taxes every ArrayList call

**Status:** OPEN (2026-08-12). The residual behind
[netty investigate-batch-03](investigate-batch-03.md)'s last failure, after the
slot-layout memo landed
(`docs/internal/fixed-suite-bugs/arraylist-slot-layout-rederived-per-call-FIXED-20260812.md`).
Azure Linux host (`20.80.105.49`), binary built from `origin/dev` `95a156234`
plus that fix.

## Two symptoms, one cause

CratonVM fabricates a map's `values()` view as an object whose runtime class
**is exactly `java.util.ArrayList`**, rather than giving the view its own class
the way the JDK does:

| expression | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `linkedHashMap.values().getClass()` | `java.util.LinkedHashMap$LinkedValues` | **`java.util.ArrayList`** |
| `hashMap.values().getClass()` | `java.util.HashMap$Values` | **`java.util.ArrayList`** |
| `linkedHashMap.keySet().getClass()` | `java.util.LinkedHashMap$LinkedKeySet` | `java.util.HashSet` |
| `arrayList.subList(0,2).getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` |
| `Arrays.asList("a").getClass()` | `java.util.Arrays$ArrayList` | `java.util.Arrays$ArrayList` ✅ |
| `Collections.unmodifiableList(…)` | `…$UnmodifiableRandomAccessList` | same ✅ |

**Symptom 1 — the class name is simply wrong.** Anything that switches on a
view's concrete type, logs it, or serialises it sees `java.util.ArrayList`.
`values()` also compares equal-by-class to a genuine list, which it is not.

**Symptom 2 — and this is the expensive one — no ArrayList operation can take
an exact-class fast path.** Because a plain `java.util.ArrayList` receiver
might *be* a values view, `native_al_size` and friends must run the full
discrimination chain on every call: `ksv_route` (keySet view?),
`unmod_receiver_backing` (unmodifiable wrapper?), `resync_values_view` (values
view — which itself computes `al_state` and reads the backing array), then
`al_state` again for the real work.

Measured on an idle box, after the memo fix:

| | HotSpot | CratonVM | vs the same body in plain Java on CratonVM |
| --- | --- | --- | --- |
| `ArrayList.size()` | 1 ns | 337 ns | **12×** (plain Java: 27 ns) |
| `ArrayList.get(i)` | 3 ns | 428 ns | **7×** (plain Java: 62 ns) |

`ArrayList.size()` is `getfield size; ireturn`. The interpreter executes that in
27 ns when it is ordinary bytecode. It costs 337 ns as a native.

**How much of that 337 ns is view discrimination is not established here.** A
sibling profile of the same VM
([adaptive-bytebuf-allocator-throughput](adaptive-bytebuf-allocator-throughput-20260812.md))
puts ~30% of CPU on call-transfer machinery — JIT entry/exit bookkeeping, the
dispatch around it, and the native-registry probe — at roughly **200 ns of
bookkeeping per entry**, paid by every native call regardless of its body. That
is a floor this fix cannot go below, and it accounts for most of the remaining
337 ns. The view-discrimination chain is what is left on top of it, and it is
the part a carrier-class change could remove.

The two findings agree on the shape of the problem: CratonVM's cost is per-call,
spread across shim families that stand in for ordinary JDK bytecode
(`Math.min(II)I` as a native is one machine instruction turned into a registry
lookup plus a call transfer). This page is one concrete, now-partly-fixed
instance of that.

## Why it matters beyond one netty test

`java.util.ArrayList` is among the most-executed classes in any Java workload,
so this is a broad multiplier, not a corner. It surfaced here because netty's
`AhoCorasicSearchProcessorFactory.buildTrie` makes 6.05 M ArrayList calls to
build its 256-entry-per-node trie: `SearchProcessorTest` now **passes solo
(15/15 in 106–111 s, twice)** but still **fails under the suite's 3-way
sharding** (135 s), tripping JUnit's 120 s per-test cap. HotSpot runs the same
class in 2.1 s.

## Suggested fix

Give the views their own carrier classes, as the JDK does — a fabricated
`java/util/HashMap$Values` (and `LinkedValues`, `LinkedKeySet`, …) rather than
reusing `java/util/ArrayList`. That fixes the `getClass()` divergence directly,
and it makes the exact-class fast path *sound*: a receiver whose class is
exactly `java/util/ArrayList` could then skip `ksv_route`,
`unmod_receiver_backing` and `resync_values_view` entirely and go straight to
the two field reads, which is where the remaining 12× lives.

**Do not add that fast path first.** It is only safe once views stop being
ArrayList-classed — this page exists because that was the tempting shortcut,
and the probe above is what ruled it out.

A larger alternative worth considering separately: in real-JDK mode, stop
registering the `native_al_*` family over a genuine `java.util.ArrayList` at all
and let the real bytecode run — it is measurably 12× faster here. That is the
same move `67c5e048c` made for `CyclicBarrier`, but with a much wider blast
radius, since these natives also serve every fabricated collection.

## Repro

```bash
javac -d . ViewClass.java ListScaleProbe.java CallCostProbe.java
cratonvm --java-home <jdk25> -cp . ViewClass       # lhm.values = java.util.ArrayList
cratonvm --java-home <jdk25> -cp . CallCostProbe   # ArrayList.size() 337 ns vs MyList.size() 27 ns

cd apps/netty-suite-runner
printf 'io.netty.buffer.search.SearchProcessorTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 900 --bin <cratonvm> --out /tmp/solo   # PASS
bash run-netty-suite.sh --list /tmp/one.txt --shards 3 --timeout 1800 --bin <cratonvm> --out /tmp/sharded # FAIL
```
