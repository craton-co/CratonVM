# `ConfigurationPropertySourcesTests` — decomposed, Term 1 fixed, ~23× HotSpot

**Status: OPEN — Terms 1 and 3 CLOSED, Term 2 RE-TAKEN, re-scoped, and one of
its three doors CLOSED, all on 2026-08-24. Term 3's fix is real but measured NOT
to move this class**, so what is left of the gap here is Term 2 alone. Of Term
2's three doors, the ITERATOR door is now fixed (1.5×, and the cost was the
fail-fast check re-deriving per element what is constant per iterator — see
"The iterator door, fixed"); it is the worst door in the table but NOT the one
this class takes. What is left is one precisely-named next step (`ArrayList.get`,
worth ~6×, on the door this class DOES take) and one open question (why
`ArrayList$Itr` bytecode is SLOWER than its native), both in Term 2 below.
Rewritten from the 2026-08-21 first cut, which called this "the
native-collections floor, no leaf over ~8%, no dominant term to attack" and left
it there. That reading was **wrong in the way that matters**: a flat profile does
not mean a flat cause. Decomposed properly, the gap is three named things, two of
which are algorithmic divergences from the JDK rather than constant-factor
overhead.

## The end-to-end number, measured

Term 1 (the map-view rebuild) was fixed on 2026-08-23 and this class was
re-measured on 2026-08-24. **One binary, `CRATONVM_MAP_VIEW_CACHE` as the A/B,
three interleaved rounds, reported in CPU time:**

| round | HotSpot | cache OFF | cache ON | OFF/ON |
|---|---:|---:|---:|---:|
| 1 | 8.3 s | 1291.0 s | 186.2 s | 6.93× |
| 2 | 6.6 s | 1274.9 s | 187.3 s | 6.81× |
| 3 | 8.4 s | 1304.3 s | 191.2 s | 6.82× |

**6.8× end to end**, and the class moves from ~156× HotSpot to **~23×**. All
three rounds pass 11/11 tests on both arms (one test is `@Disabled`, "for manual
testing", on both VMs).

**Why CPU time and not wall clock.** The box is shared and ran at load 17–37
with ten other `cratonvm` processes throughout. Wall clock there measures the
neighbours: one HotSpot run read 2.7 s idle and 15.4 s at load 35, a 5.7×
excursion on an unchanged binary. CPU time moved 1.5× over the same range, and
across the three rounds the OFF arm spans 2.3% and the ON arm 2.7% — the ON arm
also read 184.3 s at load 5, so it is flat from load 5 to 24. A first attempt at
this measurement in wall clock was discarded, not adjusted.

**The engagement counter, identical on all three ON runs** (deterministic):

```text
[MAP-VIEW-CACHE] EXIT resync_skipped=300132 resync_ran=66 elided=100.0%
                      view_reused=299758 view_built=415 switch=ON verify=OFF
```

300 132 rebuilds skipped against 66 run, and 299 758 view reuses against 415
builds — which is what says the fix is engaged on THIS workload rather than
merely present in the binary.

**The 245× in this page's old title was wall clock on a loaded host and is not
comparable to the numbers above.** The ~156× OFF figure here is the same
pre-fix behaviour priced the new way, on the same host, in the same window as
its own ON control.

What follows is the decomposition, the measurements that pin each term, and the
fixes, so the next session starts from arithmetic instead of from a profile.

## The workload, exactly

`environmentPropertyAccessWhenImmutableShouldBePerformant` and its two siblings
do 1000 property lookups across 100 `MapPropertySource`s of 1000 properties
each. Each lookup asks every source for its cache, and for a **mutable** source
`SoftReferenceConfigurationPropertyCache.hasExpired()` is true on every call
(`timeToLive == null` is the default), so `Cache.update` runs every time:

```java
String[] lastUpdated  = data.lastUpdated();
String[] propertyNames = propertySource.getPropertyNames();   //  <- (2)
if (lastUpdated != null && Arrays.equals(lastUpdated, propertyNames)) {
    return;                                                   //  <- (1)
}
```

Instrumenting that class and running it on **both** VMs gives byte-identical
control flow — this is not CratonVM taking a different path:

```
HotSpot   SICS_DIAG updateCalls=102114 fastPath=101910 rebuildFirst=204
          rebuildDiffered=0 lenDiffered=0 elemSameRef=100008000 elemEqual=0
CratonVM  SICS_DIAG updateCalls=102114 fastPath=101910 rebuildFirst=204
          rebuildDiffered=0 lenDiffered=0 elemSameRef=100018000 elemEqual=0
```

So the fast path fires 101 910 times out of 102 114 on both, and the real work
per test run is **~100 million** `Arrays.equals` element comparisons (all
resolved by reference identity) plus **~100 million** keySet steps inside
`getPropertyNames()`. HotSpot: 6–7 s. CratonVM: 475–580 s.

**Two readings refuted before writing this up.** "The Spring cache never hits
under CratonVM" — it hits exactly as often, see above. "`SoftReference.get()` is
broken" — 300 000 reads of a strongly-held referent, `nullsImmediate=0
nullsFresh=0 nullsWeak=0 identityOk=true`, identical on both VMs.

## It is not codegen, and the JIT is nearly irrelevant here

| | ms |
|---|---|
| CratonVM, JIT | 475 178 |
| CratonVM, `--nojit` | 643 523 |
| HotSpot | 6 125 |

The JIT buys **1.35×** on a workload HotSpot runs 78× faster. Whatever is
expensive is not compiled code. `perf record --sort dso` agrees: **96.09 % of
samples are inside the `cratonvm` binary**, not in JIT-compiled code.

And plain compiled Java is fine — a hand-written loop over the same two arrays:

| loop | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `a[i] != b[i]` | 1.3 ns/elem | 4.9 | 3.8× |
| body of `Objects.equals` written out | 1.4 | 7.5 | 5.4× |
| `aaload` both sides only | 2.5 | 2.7 | 1.1× |

**~5× on ordinary compiled Java, 40–400× on anything that touches the native
collections.** That is the whole story, and it is where the first cut stopped.

## Term 1 — `keySet()` is O(n) per call; the JDK's is O(1)

`native_lhm_key_set` (and the `HashMap` twin) calls `lhm_collect_keys` and then
`make_view_set_of`, which allocates a carrier, allocates a backing map, and
**inserts every key through `native_map_put`** — hashing each key and probing
for duplicates. Per call. The JDK returns a cached live view and touches
nothing.

Measured on a 1000-entry map (`KeySetBench`), µs per call:

| rung | HotSpot | CratonVM |
|---|---|---|
| `map.size()` | 0.4 | 1.8 |
| **`map.keySet()` and nothing else** | **~0** | **2619** |
| iterate a *hoisted* view | 10.8 | 4434 |
| `keySet()` + iterate | 10.0 | 6151 |

`HashMap` behaves the same as `LinkedHashMap` (2852 µs). Spring calls this
101 910 times per test run.

## Term 2 — RE-TAKEN 2026-08-24: it is not one constant, it is three doors

**The section below is the ORIGINAL 2026-08-22 reading and it is now
misleading in two ways.** Both were found by re-taking it on a post-Term-1
binary, which is the only reason they are visible: the old profile was of the
`keySet()` CONSTRUCTION rung, and Term 1 deleted that rung's cost (415 builds
per run instead of 101 910). Kept below rather than deleted, because the O(n)
scaling table is still valid and the profile is still the right profile of the
thing it profiled.

**(a) Spring does not take the door this term measured.** The `hoisted` /
`perCall` rungs iterate. `getPropertyNames()` is
`StringUtils.toStringArray(map.keySet())`, i.e. `Collection.toArray(T[])`.
Those are different natives with different costs. LinkedHashMap, width 1000,
µs per call, so 1000 elements per row:

| door | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| `keySet()` iterator (`hoisted`) | 1035-1350 | 6.5-9.0 | ~150× |
| `keySet().toArray(new String[0])` — **what Spring does** | **99-133** | 8.0-9.5 | **~13×** |
| `keySet().toArray()` untyped | 106-137 | 6.5 | ~18× |

So the per-element cost on the workload is **~0.11 µs, not ~2 µs**. The ~2 µs
number describes the iterator, which this test does not use.

**(b) It is not the map view, and it is not "the natives" as a class.** The
same loop over a plain `ArrayList` and a plain `HashSet` — no view, no
resync, no live-view machinery — costs the same order. The raw array is the
floor that says so:

| rung | CratonVM | HotSpot | vs raw array |
|---|---:|---:|---:|
| `String[]` by index (**floor**) | 18.5-24.5 | 3.5 | 1× |
| `String[]` for-each | 19.5-20.0 | 3.5 | ~1× |
| `ArrayList.get(i)` in a loop | 777-958 | 5.5-7.0 | **~37×** |
| `ArrayList` iterator | 930-1068 | 7.0-7.5 | **~45×** |
| `HashSet` iterator | 781-788 | 8.5 | ~35× |
| `keySet()` view iterator | 1022-1350 | 6.5-9.0 | ~50× |

A raw array loop runs at 20 ns/element, so the JIT compiles the loop fine and
the interpreter is not the problem. The gap is entirely in the **collection
accessor natives**, and the map view adds only ~10-25% on top of what a plain
`ArrayList` already costs.

### The lever, and why it has two signs

`java/util/ArrayList.get(I)Ljava/lang/Object;` is registered
`NativeKind::Bridge`, and `Bridge` is exactly what
`jdk_only_dial_yields_to_bytecode` yields to — under `--jdk-only`. Scoping the
dial to ONE class with `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/ArrayList`
isolates it from everything else that mode changes:

| rung | default | `--jdk-only` (all) | `--jdk-only`, ArrayList only |
|---|---:|---:|---:|
| `ArrayList.get(i)` | 958 | 145.5 | **152.5** |
| `ArrayList` iterator | 937 | 1288.5 | **4785.0** |
| view iterator | 1139.5 | 1645.5 | 1642.5 |
| view `toArray` | 99.0 | 94.0 | 122.0 |

**`ArrayList.get`'s native costs 6.3× its own JDK bytecode** — and the
ArrayList-only column matching the all-classes column is what says `get` is the
whole of that effect rather than a side effect of the mode.

**The iterator moves the other way**, and hard: arming ArrayList *alone* makes
the iterator 5× worse than default, worse than arming everything. So the JDK's
`ArrayList$Itr.next()` is slower here than the native it replaces, while
`ArrayList.get` is 6.3× faster than the one it replaces. That opposite sign is
why "the per-element constant" reads as one flat number and why no
whole-subsystem switch moved it: two effects of comparable size cancelling.
Each door has to be armed on its own evidence.

### The Term 3 treatment was ATTEMPTED for `ArrayList.get` and REVERTED

It is behaviourally viable and mechanically inert, and it broke something else.
All three parts matter to whoever picks this up.

**Behaviourally it is fine.** `probes/ListYieldProbe` (new, 44 rows) covers the
receivers that are ArrayList-SHAPED but are not plain ArrayLists, which is where
real JDK bytecode reading `elementData`/`size` straight through would diverge: a
live `values()` view across put and remove, `subList` (whose indices are offset
from the backing) including write-through, `Collections.unmodifiableList`
refusing writes, `Arrays.asList` allowing `set` but refusing `add`, iterator
order, fail-fast, bounds and null. **44/44 byte-identical to HotSpot 25.0.3+9
both on the default policy and under `--jdk-only` with the dial scoped to
`java/util/ArrayList`** — so the real bytecode does drive CratonVM's ArrayList
correctly.

**Mechanically it did not engage, and the retag half was a no-op from the
start.** The census says `java/util/ArrayList.get` is **ALREADY**
`kind: synthetic-stub` on plain dev, with `kind_stated: true` — even though
`register_arraylist_natives` sets an ambient `set_category(Bridge)` around it.
`NativeMethodRegistry::register` adjudicates the kind itself (its "two drop arms
and the `keep_real_*` heuristics"), so **the ambient category is not what
lands**, and reading the registrar to learn a triple's kind is unreliable. Use
the census.

So the only effective half was the allow-list entry, and with it in place every
precondition is satisfied and it STILL does not yield:

* `kind` is `synthetic-stub` ✓
* `real_protected_stub_class("java/util/ArrayList")` ✓ (arms C and D)
* all five terms of the yield predicate ✓ — the census's own
  `real_declaring_method` block reads
  `{loaded: true, declared: true, acc_native: false, has_code: true}`

and `invocations` stays at 127 in every arm and every mode. The refusal is
therefore in the **dispatch path**, not in the registration, the allow-list or
the predicate — and not in the JIT, since `--nojit` refuses identically.

**~~The next probe is named~~ — RUN, and it answered the opposite of what the
question assumed.** All three doors print. The arbitration was never the
problem; the instrument was. See the retraction above and the 11.1× fix that
came out of it.

What the probe left behind is `CRATONVM_DBG_STUB_DOOR=1`, a `#[track_caller]`
tally of every `real_protected_stub_class` question by CALL SITE, class and
verdict, dumped at exit. It is a tally rather than a line per call because this
predicate is on the dispatch path. It also found a **sixth** door this page had
never listed (`vm_exec.rs:19183`), which is the other reason to prefer
`#[track_caller]` over hand-placed traces: a hand-written list of doors is a
list of the doors you already knew about.

**~~And it silently disabled the Objects yield.~~ RETRACTED 2026-08-24 — that
claim was wrong.** It was published off one paired reading (`Objects.equals`
50.6 → 155.6 ns, `isNull` 28.0 → 117.0, with two local-twin controls flat) and
it does not reproduce. Bisected properly, four binaries from the same commit:

| arm | change | `Objects.equals` invocations | `ArrayList.get` invocations |
|---|---|---:|---:|
| A | dev, control | 0 | 127 |
| B | the `SyntheticStub` retag only | 0 | 127 |
| C | the allow-list entry only | 0 | 127 |
| D | **both** — exactly the reverted attempt | 0 | 127 |

Timings agree: `Objects.equals` reads 56.8-66.8 ns on D against 57.0-60.2 on A,
i.e. noise. **No arm regresses anything.** The controls being flat is what made
the original reading persuasive, and it should not have been — two rungs moving
3× with two controls flat is a coincidence, not a mechanism, and "adding one arm
to a `matches!` cannot remove another" was reason enough to withhold the claim
until it was bisected. What the original reading actually was is not known; it
was taken against a binary built at an earlier dev commit, which was not
re-tested.

**~~`invocations` is the instrument that settles this, not ns/call.~~ ALSO
RETRACTED, same day, and this one matters more than the first.** The native
census's `invocations` **SATURATES**. It reads the same `127` for
`ArrayList.get` whether the probe does 5 000 iterations or 50 000, and it
prints **`invocations_complete: false`** in the very same JSON row to say so.
I read past that field twice and built a four-arm bisect on a counter that
could not move. The four arms agreeing proved nothing whatsoever.

(The first retraction above still stands — it rests on the A-vs-D *timings*,
which were measured directly and were within noise. Only the invocation-count
argument is withdrawn.)

**And the conclusion drawn from it was wrong too.** "The arbitration is never
reached" is false. A `#[track_caller]` tally on `real_protected_stub_class`
(`CRATONVM_DBG_STUB_DOOR=1`, shipped with this) shows `java/util/ArrayList`
asked at **all three** virtual doors on a plain `ListYieldProbe` run:

```text
[STUB-DOOR] dispatch_virtual.rs:168   java/util/ArrayList -> false  x3
[STUB-DOOR] dispatch_virtual.rs:3235  java/util/ArrayList -> false  x9
[STUB-DOOR] native_override.rs:6983   java/util/ArrayList -> false  x381
```

`-> false` only because the class was not allow-listed. Arm it, and it yields.

### The iterator: two native crossings per element, and the arbitration cannot reach it

Measured 2026-08-24, and this one is an EXACT count rather than an estimate.
`probes/KeySetBench iterList` walks 2000 × 1000 elements; `--dump-native-registry`
reports:

```text
java/util/ArrayList$Itr.hasNext   kind=bridge  invocations=2002000  complete=true
java/util/ArrayList$Itr.next      kind=bridge  invocations=2000000  complete=true
java/util/Iterator.hasNext        kind=bridge  invocations=0
java/util/Iterator.next           kind=bridge  invocations=0
```

2 000 000 = exactly one `next` per element, and one `hasNext` per element plus
one per loop. **The iterator's whole cost is two native crossings per element**,
which at this VM's funnel price is the ~1 µs/element the original Term 2
measured. It is not the map view, not the JDK's `Itr` logic, and not the
`java/util/Iterator` interface fallback — that fallback (registered for
"synthetic iterator wrappers") is entered **zero** times and can be ignored.

**A read on `invocations_complete` this page got wrong once:** it is PER ROW.
These `Itr` rows say `true` and are exact; the `ArrayList.get` row says `false`
and saturates at 127. The field is trustworthy — it just has to be read.

**The allow-list does NOT fix this one, and the instrument says why.** Arming
`java/util/ArrayList$Itr` exactly as `ArrayList.get` was armed moved nothing
(`iterList` 1823/1724/1853 → 1723/1776/1850, interleaved, noise). With
`CRATONVM_DBG_STUB_DOOR=1`, `java/util/ArrayList$Itr` **never appears at any of
the six doors** — under `--nojit` as well as compiled, where the natives still
serve 20 020/20 000 calls. `ArrayList.get` was asked x3/x9/x381; the iterator is
asked zero times.

So the arbitration is genuinely not reached here, and this time that is measured
rather than inferred from a saturating counter. The difference is the CALL
SHAPE: `list.get(i)` is a virtual call on `java/util/ArrayList`, while
`it.hasNext()` is an **`invokeinterface` on `java/util/Iterator`** whose
receiver-class native (`ArrayList$Itr.hasNext`) is resolved and cached without
anyone asking `real_protected_stub_class`.

**~~Next step, and it is not another allow-list entry~~ — DONE 2026-08-24, and
the answer was not a dispatch bug. It was the KIND again, and closing it made
things WORSE.**

There is no missing arbitration on the interface path. `resolve_native_site`
(the JIT's native site cache) already refuses to cache a `SyntheticStub`,
explicitly because those "are subject to the `real_protected_stub_class` /
`has_real` yield-to-bytecode arbitration … which this path does not reproduce".
`ArrayList$Itr.hasNext`/`next` are **`Bridge`**, so they are cached and served
without any of that — and term 1 of the yield predicate is
`kind != SyntheticStub → refuse`, which is why the earlier allow-list-only arm
did nothing and why `java/util/ArrayList$Itr` never appeared at a door. The
allow-list is term 2; term 1 had already refused.

Retagging both to `SyntheticStub` and allow-listing the class does engage: the
census kind flips, and native crossings fall from ~300 000 to ~1 000 on the same
walk. **And `iterList` gets 1.56× SLOWER** — 1192.5/1219.0/1166.0 → 1830.5/
1808.0/1936.5, interleaved, every other rung flat (`hoisted`, `iterSet`,
`idxList`, `toArrHoisted`, `rawArr` all within noise).

**Why, in one line:** `CRATONVM_DBG_JIT_COMPILED` counts **zero** compiles of
`ArrayList$Itr.next`/`hasNext` in either arm — `<init>` compiles, the two hot
methods never do. So the yield trades one native crossing per element for one
*interpreted* method invocation per element, which is worse. The change was
reverted rather than shipped.

**So the earlier claim on this page that the iterator's cost IS its two native
crossings is wrong.** The count was right (2 002 000 / 2 000 000, exact); the
attribution was not. Removing the crossings does not remove the cost, so the
crossings were the symptom.

### Why the JIT never compiles it — ANSWERED 2026-08-24

**Registering a native for a method makes that method permanently
un-compilable.** Not by a refusal in the compile door — by an omission in the
call-site cache.

The invocation counter that nominates a method for tier-up lives in exactly one
place: the `CachedInvokeTarget::VirtualBytecode` arm of `dispatch_virtual`
(`profile_store.increment_invocation` → `on_method_invocation_observed`). The
**`VirtualNative` arm has no counter at all.** So a call site that resolves to a
registered native is never counted, never nominated, never enqueued, and never
compiled — no matter how hot. It is not that the compile was attempted and
refused; it was never requested.

Measured, `CRATONVM_DBG_JITC=1` on `probes/KeySetBench iterList` (2M calls each):

```text
[ir] admission   java/util/ArrayList$Itr.<init>(Ljava/util/ArrayList;)V: admitted
[cratonvm-jitc]  full-compile java/util/ArrayList$Itr.<init>...
                 …and NOTHING for next() or hasNext(), ever
```

`<init>` has no native registered and compiles. `next`/`hasNext` do, and never
appear in the log in any form.

**The converse confirms it.** `java/util/ArrayList.get`/`size` — which the fix
above made YIELD to real bytecode — now read:

```text
[ir] admission   java/util/ArrayList.get(I)Ljava/lang/Object;: admitted to the optimizing pipeline
[cratonvm-jitc]  full-compile java/util/ArrayList.get(I)Ljava/lang/Object; len=2526
CRATONVM_DBG_JIT_COMPILED: put java/util/ArrayList.get(I)Ljava/lang/Object;
```

So the 11.1× that fix bought was not merely "skip the native". It was **making
the method compilable at all** — the site flips from `VirtualNative` to
`VirtualBytecode`, which is the arm that owns the counter.

Two gates that are NOT the cause, checked and eliminated:
`CRATONVM_JIT_VIRTUAL_TIERUP` is default-ON, and `ArrayList$Itr.next()` carries
**no exception table** (`javap -c` on the real JDK class), so the
`cached.exception_table.is_empty()` precondition passes.

**What is still open, and it is now one question rather than a mystery.**
Retagging the `Itr` natives `SyntheticStub` makes them yield, so the site
*ought* to cache `VirtualBytecode` and tier up — and it measured 1.56× SLOWER
with zero compiles. `ArrayList.get` yields and does compile. So for the `Itr`
triples the yield is evidently happening on the SLOW path
(`invoke_or_native`) rather than at cache-population time, leaving the site
uncached: every call takes the generic dispatcher, which is both slower than the
native AND still has no counter. **Yielding is not sufficient; the yield has to
happen where the site is POPULATED.** Find why population declines for an
`invokeinterface`-reached `Itr` triple where it accepts `ArrayList.get`, and the
retag becomes the win the crossing count always suggested it should be.

### The fix: one allow-list entry, 11.1×

`java/util/ArrayList` joins `real_protected_stub_class_common`. MEASURED on one
tree, three interleaved rounds, µs/call at width 1000:

| rung | unarmed | armed | effect |
|---|---:|---:|---:|
| `idxList` (1000 × `list.get(i)`) | 744.5/751.5/760.5 | **67.5/67.0/68.5** | **11.1×** |
| `toArrHoisted` (Spring's door) | 113.5/108.0/110.0 | 97.0/95.0/95.5 | 1.15× |
| `iterList` (the iterator) | 1023.5/960.0/978.5 | 963.5/985.5/964.5 | flat |
| `rawArr` (**control**, no collection) | 20.0/20.0/17.5 | 17.0/18.0/20.5 | flat |

The iterator does not go through `get`, so it does not move — which is also
what says the 11.1× is `get` and not a warmer VM. An earlier reading had
`hoisted` looking 15% worse under the armed arm; on a quiet host it is
578/592/650 against 585/578/585, i.e. one noisy round.

**Two attempts were reverted before this one, both on the saturating counter,
both concluding "it does not engage".** Both were wrong. The yield worked the
whole time. Score this class by TIME; `invocations` cannot see it.

**Still open, unchanged:** ~6× is available on indexed list access if the
engagement problem is solved. Do NOT extend it to the iterator family on the
same reasoning; the number above says that would be a pessimisation, and it
needs its own investigation into why `ArrayList$Itr` bytecode is slow (the first
question being whether it is compiled at all).

### The leaf fast path is engaged and is not the lever

`native-collections` registers nothing as leaf, so the obvious next idea is to
mark the pure accessors leaf and drop the funnel's pinning, STW probe,
transitions and unwind bookkeeping. The numbers say do not bother.
`probes/NativeFunnelFloorProbe`, this host, from compiled code:

```text
control: plain Java call         10.8 ns
LEAF   AtomicInteger.get        383-417 ns
FUNNEL AtomicInteger.CAS            482 ns
FUNNEL MessageDigest.update         156 ns
FUNNEL identityHashCode             127 ns
FUNNEL System.nanoTime               90 ns
```

`CRATONVM_DBG=intrinsic-stats` confirms engagement — `compiled leaf-native
dispatches: 3918000`, so the leaf arm is running, not skipped. Yet the LEAF rung
costs 2.5-4× the full-funnel rungs beneath it. Leafness buys ~100 ns against its
own non-leaf twin (383 vs 482, same receiver and call-site shape) and that is
real, but it lands nowhere near `System.nanoTime`'s 90 ns. So the funnel is not
what makes `AtomicInteger.get` expensive, and marking collection accessors leaf
would not close a 37× gap. **Why a leaf native costs 4× a full-funnel one is its
own question**, and a better one than anything on this page.

### The iterator door, fixed — 1.5×, and it was not the natives, it was the fail-fast check

**FIXED 2026-08-24**, for the worst of the three doors in the table above (the
view iterator, ~50× HotSpot). This does not move the Spring class — the re-take
above is right that `getPropertyNames()` takes the `toArray` door — but the
iterator door is what `HashSet`, `keySet()` and every `for (x : collection)` in
the VM take, and it was carrying an avoidable 1.5×.

**The A/B that found it needed no build.** `map_itr_check_comod` — the
`HashMap$HashIterator.nextNode()` comodification test added on 2026-08-23 —
already has a kill switch, so pricing it is one environment variable on the
existing binary. Three interleaved rounds, `LinkedHashMap` width 1000:

| rung | fail-fast ON | `CRATONVM_NO_MAP_ITERATOR_FAILFAST=1` | ratio |
|---|---:|---:|---:|
| `hoisted` | 10930 / 10472 / 10872 ms | 5478 / 5469 | **2.0×** |
| `perCall` | 14357 / 13076 ms | 5707 / 4409 | **2.5–3.0×** |

**Half of everything this workload had left after Term 1 was the fail-fast
check** — which is not a reason to weaken it. It runs once per `next()`, and on
every one of those calls it re-derived three facts that are constant for the
life of the iterator:

* `resolve_field_index_by_class_id(cid, "expectedModCount")` — a name-keyed
  field resolution under the class-manager read lock;
* the comodification SOURCE, via `hs_backing_map` → `hs_map_slot`, which asks
  `class_id_by_name` twice and `is_subclass` twice;
* `get_field_by_name(src, "modCount")` — another name-keyed resolution.

`perf record -F 999` over `probes/KeySetBench hoisted` names the same chain from
the other end, and its top leaf is not in this file at all:

| share | symbol |
|---|---|
| **9.91 %** | `hashbrown::HashMap<ClassId, ()>::insert` |
| 5.37 % | `resolve_field_descriptor_byte_cached` |
| 5.01 % | `ZObjectStarts::contains` |
| 4.59 % | `Class::is_subclass_of_inner` |
| 4.39 % | `ZgcRealHeap::is_object_address` |
| 1.80 % | `ClassManager::classify_exact_name` |
| 1.57 % | `NativeContextImpl::get_field_by_name` |
| 1.45 % | `ClassManager::find_unique_class_by_name` |
| 1.27 % | `resolve_field_index_by_class_id` |
| 1.14 % | `class_id_by_name` |

The largest single leaf in the profile is the **visited set of
`Class::is_subclass_of`** — an `FxHashSet<ClassId>` allocated per subtype test,
pre-sized to 16 so it would not rehash, holding a handful of `u32`s. Reached
from `hs_map_slot`'s two `is_subclass` calls, once per element.

### The three changes

1. **`Class::is_subclass_of`'s visited set is a stack array** (`VisitedClasses`,
   32 inline `u32`s with a `Vec` spill). No allocation, no hashing; the spill
   keeps the linear bound for a pathological hierarchy rather than losing the
   dedupe and going exponential, which is what the set exists to prevent. This
   is VM-wide — the same set is on the JIT invoke path.
2. **The three name lookups are memoized per `(vm identity, ClassId)`** in a
   direct-mapped 128-slot per-thread table. A collision or a second `SharedVm`
   in one process is a MISS, not a wrong answer; the VM identity is in the key
   because `ClassId`s are per-VM dense indices. **Only POSITIVE answers are
   cached** — these are pure functions of the class STORE and the store grows,
   so a `None` from `class_id_by_name("java/util/HashSet")` before that class is
   loaded must not be pinned for the process.
3. **`native_hs_iterator` collects the snapshot ONCE.** It used to collect for
   the length, allocate the array, and collect AGAIN because `alloc_ref_array`
   may have moved everything the first collect produced — and then hand the
   second collect's results to `pin_value_slice`, which is the other way to
   survive an allocation and the one this file uses everywhere else. Pinning the
   FIRST collect makes the second walk pure waste: 1000 nodes × 2 `get_field`
   calls per `iterator()`.

### Measured

Two binaries from ONE tree (the control is the same merge with only these four
files stashed), ABBA-interleaved, minimum per round, `LinkedHashMap` width 1000,
2000 calls:

| rung | round 1 | round 2 | round 3 |
|---|---|---|---|
| `hoisted` before → after | 5248 → 3543 | 5432 → 3720 | 5254 → 3389 |
| `perCall` before → after | 4837 → 3353 | 4914 → 3248 | 6358 → 3282 |

**1.44–1.94×, six rounds out of six.** The comodification behaviour is
unchanged and that is checked rather than argued: `probes/MapModCountProbe`
reports **CME in all twelve cells and both `LIVEVIEW` rows, byte-identical to
HotSpot 25.0.3+9**, i.e. the 16-of-16 state `a6911c502` reached is preserved.
That control is not decoration — an earlier cut of this work regressed
`TreeMap.entrySet()` from CME to NONE and nothing else would have caught it.

## Term 2 (ORIGINAL 2026-08-22 reading) — the per-element constant is ~2 µs, and it is linear

The obvious next guess is a quadratic iterator. It is not. Holding total
elements fixed and scaling the map width:

| width | `keySet()` build | iterate hoisted |
|---|---|---|
| 250 | 1.84 µs/elem | 2.39 µs/elem |
| 500 | 1.72 | 2.54 |
| 1000 | 1.95 | 2.32 |
| 2000 | 1.88 | 2.24 |

Flat. Both are clean **O(n) with a ~2 µs per-element constant** — against
HotSpot's ~10 ns. That constant is the floor, and profiling the isolated
`keySet()` rung (no Spring, 13 s of pure view construction) shows what it is
made of:

| share | group |
|---|---|
| 17.4 % | object-address validation (`ZObjectStarts::contains`, `is_object_address`) |
| 28.8 % | field index/descriptor resolution + checked cell read/write |
| 7.3 % | native root pinning (`read_native_pin`, `pin_native_root`) |
| 4.9 % | `load_and_forward_inner` |
| 6.7 % | the map internals themselves |

i.e. the natives drive Java objects through a **validated, name/descriptor-resolving
heap API**, and pay that per field touch. No existing switch moves it much —
`CRATONVM_GC_NO_VALIDATE_ONCE=1` costs 6.6 % (so "validate once" is currently
worth ~6 %), `CRATONVM_COMPACT_REF_FIELDS=0` gains 6.8 %, and the Generational
collector is 33 % faster than ZGC on the same code.

## Term 3 — FIXED 2026-08-24 — a static with a registered native could never be JIT-compiled

**The diagnosis below is right about the symptom and wrong about the door, and
the wrong door is why the first fix attempt measured nothing.** `dispatch_static`
does arbitrate native-vs-bytecode — on `NativeKind`, before any of this page's
reasoning about `force_native_over_real_jdk_bytecode` applies. The question was
never "which gate does it consult", it was "what kind are these natives".

`java/util/Objects` is registered **twice**. `register_synthetic_overrides`
installs it `Intrinsic`, which is right: on a synthetic image those bodies ARE
the implementation. But `register_annotation_overrides` — reached from
`register_essential_natives_with_shims`, i.e. the REAL-JDK path — installs the
same bodies as a partial-stub-boot FALLBACK, and that one was `Intrinsic` too,
so it won unconditionally. The three stubs registered immediately above it
(`StringJoiner`, `EnumSet`, `Instant`) are all `SyntheticStub` with comments
saying real bytecode must win once the real class loads. Objects was the one
that was not.

Fixed by giving the registrar its caller's kind and adding `java/util/Objects`
to `real_protected_stub_class_common`, which arms the existing five-term yield
predicate — class loaded, not itself a compatibility stub, method resolves to a
non-native non-abstract body with Code decoded. MEASURED, 10M calls, ns/call,
interleaved, against a byte-identical local static as the control:

| rung | before | after | local twin (control) |
|---|---:|---:|---:|
| `Objects.equals` | 159 | **45.4** | 46 → 46 |
| `Objects.hashCode` | 180 | **51.3** | 50 → 50 |
| `Objects.requireNonNull` | 129 | **47.8** | 47 → 46 |
| `Objects.isNull` | 105 | **29.4** | 24.7 → 25.3 |

2.7-3.6×, each landing ON its local twin, and the four control rungs do not
move. `CRATONVM_DBG_STUB_YIELD` prints `yield=true — real bytecode wins` on the
fix and nothing on the base; `CRATONVM_DBG_JIT_COMPILED` counts 0
`java/util/Objects` entries before and 6 after — which is the engagement
evidence this page asked the next session to get before trusting any flag.

**AND IT DOES NOT MOVE THIS CLASS.** Measured end to end after landing it, same
harness and same CPU-time instrument as the Term 1 measurement at the top:

| | HotSpot | CratonVM |
|---|---:|---:|
| Term 1 only (earlier window) | 8.3 / 6.6 / 8.4 s | 186.2 / 187.3 / **191.2** s |
| Term 1 + Term 3 | 10.0 / 8.3 / 8.4 s | 201.2 / 189.9 / **189.4** s |

189.9 against 187.3, with each arm's own spread being 186-191 and 189-201. That
is a null result, and the HotSpot control is what licenses reading the two
windows against each other at all: its median is 8.4 s here and 8.3 s there, so
the windows cost the same in CPU terms even though the box was at load 33-51 for
one and 17-37 for the other. (It is still a CROSS-BINARY comparison — Term 3 is
a registration KIND and has no kill switch — so it is weaker than the
one-binary A/B above it, and is quoted only to bound the effect, not to price
it.)

**So the term table below is wrong about this term.** It priced
`Objects.equals` at ~21 s of ~475 s and called it "~4%". Making those methods
3.4x faster should then have been worth ~8% of the post-Term-1 187 s, and
nothing of the kind shows up. Either `Objects` is not a meaningful share of what
remains after Term 1, or the ~215 ns/call the estimate was built on was measuring
the pre-Term-1 profile's `Arrays.equals` path rather than these statics. The
useful conclusion is the general one: **Term 3 is a real VM-wide win and a
correctness fix, and it is NOT a fix for this class.** `requireNonNull` being
among the most-called methods in the JDK is what justifies it, not this page.

**It also fixed three wrong answers.** `probes/ObjectsYieldProbe` (50 rows,
diffed against HotSpot 25.0.3+9) is byte-identical after the fix; before it, the
native invented its own NPE messages — `"defaultObj must not be null"` where the
JDK says `"defaultObj"`, and likewise for `supplier` and `supplier.get()`. A
throughput fix that also removes three behavioural divergences is the sign the
native should not have been winning in the first place.

## Term 3 (ORIGINAL 2026-08-22 reading) — a static with a registered native can never be JIT-compiled

`java.util.Objects.equals` is registered as a `NativeKind::Intrinsic` native.
`java/util/Arrays.equals(Object[],Object[])` is **not** a native — it is
ordinary JDK bytecode, and it *is* compiled (`CRATONVM_DBG_JIT_COMPILED` shows
both a `put` and an `osr` body for it). Yet it runs at 214–224 ns/element while
the byte-identical hand-written loop runs at 7.5:

| rung | CratonVM ns/elem |
|---|---|
| `java.util.Arrays.equals` | 213.7 |
| the same loop calling `java.util.Objects.equals` | 231.5 |
| the same loop calling a **local** static with the identical body | **21.1** |
| the body written out inline | 7.5 |

`java/util/Objects.equals` never appears in the compiled-method list; the local
twin does. Directly, 10M calls per rung:

| | nativized | local twin | ratio |
|---|---|---|---|
| `Objects.equals` | 275.5 ns | 66.0 ns | 4.2× |
| `Objects.hashCode` | 323.3 ns | 73.7 ns | 4.4× |
| `Objects.requireNonNull` | 237.0 ns | 69.6 ns | 3.4× |
| `Objects.isNull` | 177.9 ns | 35.5 ns | 5.0× |

**Why**: the "prefer real JDK bytecode unless force-gated" rule lives in
`force_native_over_real_jdk_bytecode`, and that is read only from
`dispatch_virtual.rs` and `jit_bridge.rs` — **never from `dispatch_static.rs`**.
For a static, the registry always wins, so the real bytecode is never used, the
method is never compiled, and every call pays the full native-dispatch path.
`requireNonNull` is among the most-called methods in the JDK and in Spring, so
this is a VM-wide tax rather than a microbenchmark curiosity.

**A first attempt at the fix is recorded here because it FAILED, and the
failure is the useful part.** Making `dispatch_static` consult a curated
yield-list for `java.util.Objects` (behind `CRATONVM_NO_STATIC_REAL_BYTECODE`)
built clean and measured **nothing** — ON vs OFF was identical on the compiled
path *and* under `--nojit`. So the interpreter's `dispatch_static` is not the
only selector, or something downstream re-selects the native. Whoever picks
this up: **print an engagement counter before trusting the flag**, and start by
finding every place that chooses native-over-bytecode for a static, not just
that one. The change was reverted rather than shipped inert.

## Where the time actually goes

Per test run, against the ~475 s (JIT, loaded host, WALL CLOCK) arm this was
decomposed on. See "The end-to-end number, measured" at the top for what the
class costs now and why that section prices it in CPU time instead:

| term | est. cost | note | after Term 1 |
|---|---|---|---|
| `keySet()` view construction | ~190 s | 101 910 calls × 1000 elem × ~1.9 µs | **gone** |
| iterating those views | ~230 s | 100M steps × ~2.3 µs | **reduced** |
| `Objects.equals` via `Arrays.equals` | ~21 s | 100M calls × ~215 ns | unchanged |

Terms 1 and 2 were ~90 % of it. **`Objects.equals` is only ~4 %** — worth
fixing for the whole VM, but it is not this test's problem.

**Checked against the measurement rather than left as arithmetic.** The
estimates above predicted ~140 s remaining; the measured ON arm is 186-191 s
CPU. The estimate was low, and the residue is Term 2: what the elision removes
is the per-READ rebuild, not the per-element cost of walking a view once it is
built. `Objects.equals` (~21 s) and the ~66 rebuilds that still run are the rest.
Term 2 is now the whole of the remaining gap on this workload rather than part
of it, which is the useful thing the re-measurement establishes.

## The two fixes, in value order

1. **Make map views lazy/live** (`keySet`/`values`/`entrySet`) — **DONE
   2026-08-23**, for `keySet` and `entrySet`, by a route this page did not
   anticipate. Rather than delegating reads to the source, the view's backing
   now carries the source's modification generation and a resync whose source
   has not moved returns immediately; and `keySet()`/`entrySet()` hand back the
   instance the map already has instead of building a new one, which is also
   what HotSpot does and restores `map.keySet() == map.keySet()`.

   MEASURED on `probes/KeySetBench`, `LinkedHashMap` of 1000 entries, one
   binary with `CRATONVM_MAP_VIEW_CACHE` as the A/B: `keySet()` alone
   1674.5 → **2.5 µs/call**; `keySet().size()` — the shape
   `StringUtils.toStringArray(map.keySet())` uses — 3125 → **4.5 µs/call**;
   `keySet()` + iterate 5949.5 → **1489.5 µs/call**.

   `values()` did NOT get it: its carrier is a list with a different backing
   scheme. Neither did entrySet READS — only their construction — because an
   entrySet's contents include values and a value-replacing `put` deliberately
   does not move `modCount`.

   Kill switch `CRATONVM_MAP_VIEW_CACHE=0`, verify mode
   `CRATONVM_VERIFY_MAP_VIEW_CACHE=1`, engagement census
   `CRATONVM_DBG=map-view-cache`.

2. **Cut the per-element constant** (Term 2). The natives read and write the
   nodes *they themselves allocated*, with a known layout, through the generic
   validated accessor. A trusted-access path for that case is where the 17 % +
   29 % lives. This is a change at the native/GC boundary and needs to be
   argued for GC-correctness, not just measured.

A cheaper partial for Term 1, if a lazy view is too big a step: build the view's
backing directly from the source's `(hash, key)` node pairs — the hash is
already stored in `NODE_FIELD_HASH` and the keys are already unique, so both
`map_hash_key` and the duplicate probe are pure waste. Constant-factor only; it
does not remove the O(n)-per-call. **Still worth doing** — the 2026-08-23 fix
removes the repeated builds but not the cost of the first one, so this is what
is left of Term 1 for a map whose key set really does keep changing.

## Reproducing

Benches used here (all standalone, no Spring): `ArrEqBench`, `ArrEqBench2`,
`PropNamesBench`, `KeySetBench`, `StaticNativeBench`. The Spring-level
instrumentation was a classpath shadow of
`SpringIterableConfigurationPropertySource` counting fast-path vs rebuild — the
same "instrument the consumer" technique that cracked the BatchJdbc miscompile.

```bash
cd /data/cratonvm/apps/spring-boot/core/spring-boot
cratonvm --java-home /data/toolchain/jdk-25 --Xmx 2g --XX:UseGc ZGC \
  -cp "$(cat build/cratonvm-test-cp.txt)" SbRunner \
  org.springframework.boot.context.properties.source.ConfigurationPropertySourcesTests
```

`perf` on this host needs `sudo sysctl -w kernel.perf_event_paranoid=1` first.
**Read `/proc/loadavg` before believing any number** — this box is shared and
was seen at load 178 on 8 cores. Every ratio above is from a paired run on one
host at one load; the absolute walls are not comparable across sections.
