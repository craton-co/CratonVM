# H16-3 — the arming dial yields to real bytecode ONCE per process, and what it leaves behind is a map with both node classes in one table

> **CORRECTION 2026-08-22 — the counting claim in this record is
> withdrawn (`H17-2` N4).** The `table` array class is not a yield
> counter. It reports **one bit per map** — whether that map's FIRST
> insert ran bytecode — so it cannot distinguish "this method never
> yielded" from "this method yielded later, after the native had already
> allocated the table". Every observation here stands; every statement of
> the FORM "the dial yields once per X" does not.
>
> The mechanism is now settled and it was never a yield budget: the dial
> had one live call site of fourteen dispatch doors, so an armed class
> yielded only on the dispatches that reached step 1 cold. Fixed
> 2026-08-21 — all fourteen doors consult it, and an armed class now
> yields on every covered dispatch that has bytecode to yield to. See
> `WORKER-1-the-dial-now-reaches-every-door-20260821.md`.

**Status: OPEN — MEASURED.** Every number is from the prebuilt
`C:/craton/cratonvm-r5.exe` (2026-08-20 21:57), `--jdk-only`, against HotSpot
25.0.3+9. **No source change was involved in any measurement here** and no
suite was run — these are single-class probes, reflectively reading
`java.util.HashMap.table` and `java.util.Hashtable.table` with
`--add-opens java.base/java.util=ALL-UNNAMED`.

Lane H16, 2026-08-21. Bounds what `H0-4` §1 and `H14-3` §1's armed cells
measure. Also carries a live unarmed defect found on the way (§4).

---

## 1. MEASURED — the armed table is a HYBRID, and exactly one node in it is real

`HmTableProbe`: `new HashMap()`, `n` string puts, then reflectively read
`table`. `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap`, `--jdk-only`.

| n | `table` class | first three occupied buckets | buckets used |
|---:|---|---|---:|
| 1 | `[Ljava.util.HashMap$Node;` len 16 | `HashMap$Node` | 1 |
| 2 | `[Ljava.util.HashMap$Node;` len 16 | `HashMap$Node` · `AnonymousObject$4` | 2 |
| 3 | `[Ljava.util.HashMap$Node;` len 16 | `HashMap$Node` · `AnonymousObject$4` · `AnonymousObject$4` | 3 |
| 8 | `[Ljava.util.HashMap$Node;` len 16 | `HashMap$Node` · `AnonymousObject$4` · `AnonymousObject$4` | 8 |
| **13** | **`[Ljava.lang.Object;` len 32** | `HashMap$Node` · `AnonymousObject$4` · `AnonymousObject$4` | 10 |
| 20 | `[Ljava.lang.Object;` len 32 | `HashMap$Node` · `AnonymousObject$4` · `AnonymousObject$4` | 15 |
| 3000 | `[Ljava.lang.Object;` len 4096 | all `AnonymousObject$4` | 1881 |

Read it as a sequence and it tells the whole story:

1. **The FIRST `put` runs real bytecode.** It allocates the real
   `HashMap$Node[16]` table and one real `java.util.HashMap$Node`.
2. **Every later `put` runs the native**, which writes an
   `AnonymousObject$4` into that real `Node[]`. There is no
   `ArrayStoreException` because a native's `set_array_element` does not take
   the store check real `aastore` would.
3. **At n = 13** the load factor (16 × 0.75) is crossed and the **native's**
   `map_resize` runs, replacing the real `Node[32]` with a plain
   `Object[32]`. The real first node is copied over and survives; the array's
   type does not.

The oracle, for the same probe, is uniform at every `n`:
`table cls=[Ljava.util.HashMap$Node;`, every occupied bucket a
`java.util.HashMap$Node`, `isEntry=true`.

**UNARMED** is also uniform, and uniformly wrong: `[Ljava.lang.Object;`, every
bucket an `AnonymousObject$4`, `isEntry=false`. That is the state `H16-2`
changes.

## 2. MEASURED — the yield is ONE PER PROCESS, not one per call site and not one per receiver

`HmSitesProbe` — three DISTINCT `put` call sites into one map, then a SECOND
map with its own loop, all in one process:

```text
ARMED, three distinct callsites   size=3  tableCls=[Ljava.util.HashMap$Node;
                                  realNodeHeads=1  fabricatedHeads=2
ARMED, second map, 1 callsite ×3  size=3  tableCls=[Ljava.lang.Object;
                                  realNodeHeads=0  fabricatedHeads=3
UNARMED, either                   realNodeHeads=0  fabricatedHeads=3
```

Three call sites still produce **one** real node, and the second map produces
**none and never gets a real table at all**. So the yield is not per call site,
not per receiver, and not per map — it happens exactly once, on the first
`HashMap.put` dispatch the process performs, and never again.

`--nojit` is identical, so this is not tier-up. (Same control, same answer, as
`H0-6` §8 got on the substitution count.)

### 2a. ARGUED — the mechanism, from the tree's own doc comment

`vm/src/runtime/interpreter/native_override.rs` above
`force_native_over_real_jdk_bytecode` states it without knowing it was stating
this:

> *`try_stackless_invoke` step 1 is [the first site to answer], for nearly every
> call in the VM, and it goes through `resolve_step1_native` … where `enforce`
> is `env_cache::jdk_only_enforce_shadow_for(class_name)`.*

and, two paragraphs down, about the *later* doors:

> *this function is called on every vtable miss and every JIT bind, and **its
> result is memoized per `CachedBytecodeMethod`** … That is the failure the
> `java/lang/String` arm was deleted for on 2026-08-04 (a method's behaviour
> started depending on how many times its call site had run).*

The dial is consulted at step 1. The **vtable inline cache** is a second door
whose entry is memoized per resolved method — process-wide, not per call site —
and it does not consult the dial. First dispatch: step 1, dial honoured,
bytecode runs. Every dispatch after: the cached entry, native runs.

**MEASURED: one yield per process. ARGUED: the inline-cache memo is why.** I
did not instrument the dispatcher and did not read the cache's code; this is
the tree's own description matching the observation.

## 3. MEASURED — it generalises to a second family

Same probe against `java.util.Hashtable`,
`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Hashtable`, four puts:

| | `table` class | heads |
|---|---|---|
| HotSpot | `[Ljava.util.Hashtable$Entry;` | 4 real `Hashtable$Entry` |
| CratonVM unarmed | `[Ljava.lang.Object;` | 4 `AnonymousObject$4` |
| CratonVM armed | **`[Ljava.util.Hashtable$Entry;`** | **1 real `Hashtable$Entry`, 2 fabricated heads** |

Same shape, second family, so it is not something about `HashMap` in
particular. **Two families is not "every prefix"** — see §5.

### 3a. A separate divergence this probe exposes, not investigated here

Unarmed CratonVM puts `Hashtable`'s four entries in buckets **5, 6, 7, …**;
HotSpot puts them in **0, 1, 2, 3**. `Hashtable` indexes with
`(hash & 0x7FFFFFFF) % table.length` and `HashMap` with `hash & (cap-1)`; this
file's `map_bucket_index` implements only the second. That makes CratonVM's
`Hashtable` **iteration order** differ from HotSpot's, which is separately
visible: for a two-entry `Hashtable`, `entrySet().iterator().next()` yields
`a` on CratonVM and `b` on HotSpot. Filed as N3.

## 4. MEASURED — `ConcurrentHashMap.forEachEntry` was a live wrong answer with NO dial armed

Found while enumerating this file's sentinel sites. `--jdk-only`, unarmed:

```text
HotSpot   entry cls=java.util.concurrent.ConcurrentHashMap$Node key=k1 val=v1
          entry cls=java.util.concurrent.ConcurrentHashMap$Node key=k2 val=v2
          forEachEntry visited=2
CratonVM  forEachEntry THREW java.lang.ClassCastException: class
          cratonvm.synthetic.AnonymousObject$4 cannot be cast to class
          java.util.Map$Entry … after 0
```

The registrar allocated `alloc_object(ClassId::new(0), NODE_NUM_FIELDS)` and
wrote key/value at the NODE slots (1 and 2), leaving `hash` and `next`
unwritten — so it was not even a well-formed node, and the carrier implements
nothing. **Every call failed**, for any consumer that touches its argument,
which the parameter type `Consumer<Map.Entry>` makes every consumer.

Fixed in `292dcab08` by minting the live `AbstractMap$SimpleEntry`
(`alloc_live_entry`, with `this` as the source so `setValue` writes through)
that every other entry-yielding path in this file already uses. It is **not** a
`ConcurrentHashMap$Node`: a consumer that downcasts to the concrete node class
still fails, and that is a smaller and louder gap than the one it replaces.

`forEachKey` and `forEachValue` hand out raw keys and values and are unaffected;
they were checked. No other CHM bulk op in the file mints a carrier.

## 5. What this does NOT establish, and the correction it forces

* **It does not say the armed cells are wrong.** Every number in `H0-4` §1 and
  `H14-3` §1 is a correct measurement of the state the dial produces.
* **It says that state is not a retirement.** A retirement removes the
  registration, so the native never runs and every node in the table is real.
  What the dial produces for these two families is *one* real operation followed
  by natives writing into the structure that operation built — a **hybrid that
  neither the pure-native nor the pure-bytecode state can reach.** For
  `HashMap` that hybrid is a `Node[]` holding non-`Node`s, which is a shape
  HotSpot cannot represent at all.
* So an armed cell is an **upper bound on damage that includes hybrid damage**,
  not an estimate of retirement cost, and the two can differ in either
  direction: a hybrid can be worse than either pure state (a `Node[]` full of
  fabrications) or better (one real node survives where a retirement would have
  had to build all of them).
* **Two families, one method each.** I tested `HashMap.put` and
  `Hashtable.put`. I did NOT test another method on those classes, and I did
  NOT test any of `H14-3`'s thirteen registrars, `all`, or a prefix outside
  `java/util`. **The generalisation is NOT MEASURED.** `H14-3`'s spread
  (Properties 65/104, `register_essential_natives_with_shims` 28/104) is too
  wide to be an artefact of this alone.
* **I did not read the inline cache's source**, so §2a's mechanism is the
  tree's own prose matching my observation, not a code reading.
* **I ran no suite.** `.guard-tmp` is a fixed shared path and `H14` measured two
  concurrent sweeps moving a result from 83/104 to 102/104; with no way to
  detect another lane's run, re-measuring an already-published baseline was not
  worth the collision.

## 6. NOMINATIONS

* **N1 — every armed cell needs a hybrid check, and it is one command.** Before
  quoting a cell, run the family's structure probe (reflect the backing field,
  count real vs fabricated heads). If the count is "one real, N fabricated", the
  cell is pricing a hybrid. This is cheap enough that it should be part of
  `scripts/jdk-only-blast-radius.sh`'s output rather than a separate exercise.
* **N2 — decide whether the dial should yield ONCE or ALWAYS, and say which in
  its doc comment.** If the intent is "price a retirement", the inline-cache
  memo has to be dial-aware or invalidated when the dial is set; a once-only
  yield prices something nobody asked for. If the intent is only "observe that
  bytecode exists", then §1's hybrid is a side effect that should be documented
  where the numbers are quoted. `env_cache.rs`'s comment says *"this dial
  exists so that migration can re-take the measurement one subsystem at a
  time"* — which is the first intent, and the current behaviour does not serve
  it.
* **N3 — `Hashtable` uses the wrong bucket index.** `(hash & 0x7FFFFFFF) %
  table.length`, not `hash & (cap-1)`. Measured as a bucket-placement and
  iteration-order divergence against HotSpot (§3a). It is in
  `native-collections/src/lib.rs` (`map_bucket_index`), which this lane owns,
  and it is deliberately NOT fixed here: `Hashtable` shares `map_bucket_index`
  with `HashMap` and every other bucket-mapped family, so splitting it is its
  own change with its own arm, and stacking it under an unbuilt node-class
  change would make both unattributable.
* **N4 — the `table` array's component type.** `[Ljava.lang.Object;` where the
  class declares `[Ljava/util/HashMap$Node;`, unarmed, always. It survives only
  because the `b'['` coercion arm passes any `Object` through. Fixing it
  requires `H16-2`'s guard to be total first, or the array store will refuse the
  fallback carriers. This is `H16-2` N1 and is repeated here because it is what
  the armed n=13 row is about.

---

## INDEPENDENT REPRODUCTION (lane H0, 2026-08-21) — confirmed, with the hybrid table photographed

Probe: three `HashMap`s in one process, three puts each, reading `HashMap.table`
reflectively (`--add-opens java.base/java.util=ALL-UNNAMED`) and classifying
every bucket. Binary `cratonvm-r6.exe`, oracle HotSpot 25.0.3+9.

| | `table` array class | real `HashMap$Node` | fabricated | null |
|---|---|---:|---:|---:|
| **HotSpot**, all three maps | `[Ljava.util.HashMap$Node;` | 3 / 3 / 1 | 0 | — |
| **CratonVM unarmed**, all three | `[Ljava.lang.Object;` | **0** | 3 / 3 / 1 | — |
| **CratonVM armed, map1** | **`[Ljava.util.HashMap$Node;`** | **1** | **2** | 13 |
| **CratonVM armed, map2** | `[Ljava.lang.Object;` | 0 | 3 | 13 |
| **CratonVM armed, map3** | `[Ljava.lang.Object;` | 0 | 1 | 15 |

**This record's claim is confirmed exactly.** The yield happens **once in the
whole process** — the first put of the first map — and everything after it
reverts to the fabricated path.

### The hybrid is a state no retirement can produce, and it may be worse than either endpoint

`map1` armed is a **real `Node[]` holding one real `Node` and two
`AnonymousObject$4`s.** That is not "half retired". It is a table that is
internally inconsistent in a way neither the current VM nor a fully-retired VM
would ever produce:

* the **unarmed** state is uniform — `Object[]` full of fabrications, which is
  wrong but self-consistent, and `size()`/`get()` work (`H0-4` §7);
* a **fully retired** state would be uniform the other way — `Node[]` full of
  real nodes;
* the **armed** state is neither, and an array store of a fabricated node into a
  real `Node[]` is exactly the `ArrayStoreException` in `H0-6` §7.

### What this does to every armed number in this directory

**`CRATONVM_ENFORCE_NATIVE_SHADOW` is the instrument the entire effort has
priced with**, and this says it does not simulate a retirement. It simulates
*one* retirement, once, and then stops.

Affected, and this is not a small list: `H0-4`'s six-family table (81/104 for
`HashMap` and the rest), `H0-3`'s CHM eleven, `H14-3`'s **thirteen** arms
including the five "free" registrars and `Properties` at 65/104, and every
`H15`/`H22` armed measurement.

**I am not claiming those numbers are too high or too low, because the direction
is not knowable from this.** A hybrid table can be worse than uniform-fabricated
(a real `Node[]` rejects a fabricated store that an `Object[]` accepts) *or*
better (one real node is one fewer fabrication). **What can be said is that they
do not measure what they were read as measuring**, and that "arming costs N
vectors" is not the same proposition as "retiring costs N vectors".

### Two things this does NOT overturn

* **`H22`'s refusals stand and get stronger.** `StringBuilder` losing every
  append silently, and `Throwable` costing 61 of 61 stack traces, are *observed
  wrong behaviours* under the dial. A defect found in a hybrid state is still a
  defect found; it is the clean *zeros* that become unreliable, not the
  failures. A cell that says "this breaks" is more trustworthy than a cell that
  says "this is free".
* **The unarmed row is untouched**, and it carries its own finding: **the
  `table` array is `[Ljava.lang.Object;` in every unarmed case**, where JDK 25
  declares `[Ljava.util.HashMap$Node;`. That is the `new_ref_array` half of the
  sentinel census (`H0-6` §10) — 29 sites tree-wide — and it is a defect in the
  shipping default configuration, not only under the dial.

### NOMINATION

**N1 — find why it yields once.** A process-global latch is the obvious suspect,
and this repository has a standing note that *a process-global `OnceLock` latches
a guess forever*. If the yield decision is memoised per process rather than per
call, that is a one-line class of bug with a very large blast radius on the
instrument, not on the VM.
