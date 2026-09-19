# H16-2 — the map's chain nodes are the real `java/util/HashMap$Node`, and the residual that blocked it for 17 days was two dead hypotheses

**Status: LANDED, NOT BUILT AND NOT RUN.** This lane may not build; the
orchestrator does. Every runtime number below is from the prebuilt
`C:/craton/cratonvm-r5.exe` (2026-08-20 21:57), which **does not contain this
change** — so nothing here validates the change, and the section that says what
would falsify it (§6) is the point of the record. Oracle is HotSpot 25.0.3+9 at
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot`.

Lane H16, 2026-08-21. Commit `292dcab08`, `native-collections/src/lib.rs` only.
Acts on `H0-6` §7, `H0-4` §7, `H4-1` O2 and `H9-1`.

---

## 1. The three production sentinel sites in this file, and what each caller meant

`H0-6` §3 lists this file at "four of eighty-four". The real production
population is **three**, and `H16-1` explains why the counted four and the real
three are different sets. Test-module sites (six, all inside
`mod lbq_blocking_tests`) are excluded — a fixture fabricating a carrier is the
fixture's business.

| line (pre-change) | what the caller meant | real JDK 25 layout | verdict |
|---|---|---|---|
| `11874` `native_map_put_evict_pinned` | `java/util/HashMap$Node` | `final int hash; final K key; V value; HashMap$Node next` — 4 slots, and the slot ORDER already matched | **FIXED here** for object-shaped mappings |
| `50910` `ConcurrentHashMap.forEachEntry`'s carrier | a `java.util.Map$Entry` to hand a `Consumer` | HotSpot passes the live `ConcurrentHashMap$Node` | **FIXED here** — see `H16-3` §1, it was a live unarmed `ClassCastException` |
| `50440` `chm_init_segments` | **nothing** — a CHM *segment*, a CratonVM-only concept with no JDK class | n/a | left alone, and it is the one site where the sentinel is honest |

The 50440 row matters for the plan: it is a **VM bookkeeping type**, which
`vm_exec.rs:12997` says is legitimate in both modes (`H0-6` §6). It should never
be counted as a migration target, and a census that cannot tell it apart from
the other two will keep reporting it as one.

For completeness, the node/entry carriers this file mints through
`try_alloc_synthetic` (which DOES bind a real class) rather than the sentinel:

```text
  9858  java/util/HashMap$Node                    (map_alloc_node — view/snapshot sets)
 38531  java/util/LinkedHashMap$Entry             (lhm_alloc_node)
 35789  java/util/LinkedList$Node                 (real: item, next, prev — 3, matches)
 46835  java/util/AbstractMap$SimpleImmutableEntry (tm_make_entry, detached)
 15211  java/util/AbstractMap$SimpleEntry          (alloc_live_entry, entrySet views)
 13602 · 13695 · 15211 · 39695   java/util/Map$Entry, width 3
```

**`java/util/Map$Entry` is an INTERFACE** (`javap -p`: `public interface
java.util.Map$Entry`). Four sites allocate an instance of it. That is the
`no-code-attribute-means-abstract-receiver` / `nio-channels-are-abstract-classed`
species and HotSpot cannot produce such an object at all. Not touched here —
nominated in §7.

## 2. MEASURED — the residual that blocked this line, hypothesis 1: DEAD

The line carried a comment (`f8aff25bb`, 2026-08-04) saying the flip was
MEASURED *"necessary and NOT sufficient"* — that on top of the marker fix it
still failed `probes/LinkedHashMapNodeProbe.java` and `SetSurface`, with
*"`Map$Entry.getKey()` coming back null and `keySet().remove` leaving the map
unshrunk"*, and that *"whatever else this node's real descriptors change has not
been chased down."*

The obvious mechanism for the first half — and the one I filed this lane's plan
under — is that `java/util/Map$Entry` is registered as an **interface native**
in this very file (`registry.register("java/util/Map$Entry", "getKey", …,
native_entry_get_key)`), and `native_entry_get_key` is
`Ok(Some(ctx.get_field(this, 0)))`. On a *carrier* (key@0, value@1, source@2)
that is the key. On a **real `HashMap$Node`** slot 0 is `hash:I`, and the
`Ljava/lang/Object;` return descriptor then coerces the `Int` to null. A real
`HashMap$Node` implements `Map.Entry`, so the door is reachable the moment the
node becomes real. That is a complete, self-consistent explanation of
"`getKey()` comes back null".

**It is wrong.** Arm:

```java
// NodeDoorProbe — run with --add-opens java.base/java.util=ALL-UNNAMED
Class<?> nodeCls = Class.forName("java.util.HashMap$Node");
Constructor<?> c = nodeCls.getDeclaredConstructor(int.class, Object.class, Object.class, nodeCls);
c.setAccessible(true);
Object node = c.newInstance(1234, "KEY", "VAL", null);
Map.Entry<Object,Object> e = (Map.Entry<Object,Object>) node;
System.out.println("iface getKey  =" + e.getKey());
System.out.println("iface getValue=" + e.getValue());
Method gk = nodeCls.getMethod("getKey"); gk.setAccessible(true);
System.out.println("reflect getKey=" + gk.invoke(node));
System.out.println("hashCode=" + e.hashCode());
Object old = e.setValue("NEW");
System.out.println("setValue old=" + old + " nowValue=" + e.getValue() + " nowKey=" + e.getKey());
```

| | HotSpot 25.0.3+9 | CratonVM `--jdk-only` |
|---|---|---|
| `node.getClass()` | `java.util.HashMap$Node` | `java.util.HashMap$Node` |
| `instanceof Map.Entry` | true | true |
| `e.getKey()` (invokeinterface) | `KEY` | **`KEY`** |
| `e.getValue()` | `VAL` | **`VAL`** |
| reflective `getKey()` | `KEY` | **`KEY`** |
| `e.hashCode()` | `26942` | **`26942`** |
| `e.setValue("NEW")` | old `VAL`, key still `KEY` | **identical** |

**Byte-identical, including the hash.** The interface row does not open for a
receiver whose own class declares the method — which is `H11`'s "dispatch keys
on the receiver", now measured from the other side. Recorded because it is a
door three records have speculated about and none had knocked on.

## 3. MEASURED — hypothesis 2 (the JIT direct helper): also DEAD

`H4-1` §1c names `jit_hashmap_put_direct` / `jit_overlay_hashmap_put` as a
"second entry into the map's write path that the registry has never heard of"
and predicts a **tier-dependent wrong answer**. That is a live candidate for any
armed-mode residual, so it was tested rather than assumed.

`HashMapArmedStressProbe`, `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap`,
`--jdk-only`:

```text
with JIT      size=3000  get-back=3000  entrySet THREW CCE after 0  keySet=1
with --nojit  size=3000  get-back=3000  entrySet THREW CCE after 0  keySet=1
```

**Identical.** Same shape as `H0-6` §8's `--nojit` control on the substitution
count. Tier-up is not the explanation for anything measured in this cluster so
far, twice now, from two different instruments.

## 4. What the 2026-08-04 measurement is, and is not

It is a real measurement and I am not disputing what it saw. What it cannot be
is **evidence about this tree**:

* Its `HashSet.remove`-reports-`false` half is the PRESENT marker, and
  `11798a8a2` (`hs_present_marker_at`, lane `H9`, 2026-08-20) landed the real
  `java.util.HashSet.PRESENT` at all six HashSet-family producers — **sixteen
  days after that measurement was taken.**
* Its `Map$Entry.getKey()`-is-null half now has one candidate mechanism
  measured and disproved (§2) and one more (§3), and no surviving one.

So the residual was never localised; it was named and then quoted for
seventeen days as though it had been. `H0-4` §7 records the same species one
level up — a symptom reasoned to a cause without running the two cheap
discriminators. **A "necessary but not sufficient" finding whose insufficiency
is not localised has a shelf life, and it should carry the date and the tree it
was taken on, in the comment, not only in the commit.** The replacement comment
does.

**The guard is green today, which is what makes a red attributable.**
`probes/LinkedHashMapNodeProbe.java` — the probe `f8aff25bb` named, whose
set-membership and map-view sections it said go "loudly red (9 failures) the
moment this line changes" — **PASSES on `cratonvm-r5.exe` under `--jdk-only`**,
all eleven sections, `PROBE PASS`. Measured this lane, before the change.

## 5. What landed, and the boundary the guard draws

`native_map_put_evict_pinned` now allocates through

```rust
let node_cid = map_node_class_for(ctx, key_val, value);
let new_node = ctx.alloc_object(node_cid, NODE_NUM_FIELDS);
```

* `hm_node_class_id` resolves `java/util/HashMap$Node` through the file's
  existing **`chm_real_class`**, which already refuses a fabricated stand-in
  (`is_class_synthetic_stub` plus the name the id resolves BACK to) — the
  `ensure-class-initialized-fabricates-instead-of-failing` trap, handled by code
  that was already here. Binding a node to a *fabricated* `HashMap$Node` would
  be strictly worse than the sentinel, which at least declares no descriptors.
* The answer is memoised in a `vm_identity`-scoped thread-local `Cell`, the
  same scoping `RECEIVER_FACTS` uses and for the same stated reason. The
  **negative is cached too**: on a `synthetic-jdk` build there is no such class,
  and re-asking on every put is the cost the memo exists to remove. A
  process-global `OnceLock` would latch the first VM's answer forever.
* The hot path after the first put per VM per thread is one `Cell` read. No
  lock, no `class_name_of_id` `String`, no Java dispatch. This is deliberate:
  the file's own PERF comments record four separate `class_name_of_id` walks
  being hunted off this exact line.

**The guard.** A mapping whose key or value is a non-`Object` `Value` keeps the
untyped carrier. That is not a compromise, it is the boundary between a
Java-visible mapping and a Rust-private one:

* a real node declares both slots `Ljava/lang/Object;`, and
  `coerce_field_value_by_descriptor`'s `b'L'` arm nulls a primitive written
  there — a rule `gc/src/heap.rs` calls *"DELIBERATE AND LOAD-BEARING"* and
  pins with a test;
* but a primitive `Value` in a map is **already** unreadable from Java, because
  the same coercion nulls it at `Map.get`'s `Ljava/lang/Object;` return
  descriptor. No bytecode can tell which class carries it.

Two producers reach the put path with a primitive, and neither is hypothetical:
`present_marker` keeps a legacy `Int(1)` for a NULL set element (both
`native_hs_add` and `native_hs_remove` settle that one case with `containsKey`
instead, and say so), and `materialize_hm_int_fast` re-puts every side-stored
entry with the `Value` its writer supplied — `try_hm_int_fast_put` stores that
`Value` verbatim, and `jit_hashmap_put_direct`'s own comment counts *"42 direct
Rust `native_map_put_pub` call sites that are not obliged to store an object."*

`chm_publish_real_table`, one screen over in this same file, already applies
exactly this rule in exactly this direction: *"a primitive in a slot a real
`$Node` declares as a reference … returns `None` and the caller keeps today's
snapshot carrier verbatim. A partially-materialised `table` would be a
populated-looking lie."*

**The standing existence proof** is next door: `lhm_alloc_node` has bound its
node to the real `java/util/LinkedHashMap$Entry` since `7bf427af1`, and
`LinkedHashMapNodeProbe`'s set-membership, map-view, serialization and
2000-entry-resize sections all pass over it (§4).

## 6. PREDICTIONS, and what falsifies each

Everything here is ARGUED. None of it has been run against a binary containing
the change.

### 6a. The cheapest arm, and the one to run first

`HmTableProbe` — reflectively read `java.util.HashMap.table` after two puts.
On `cratonvm-r5.exe`, `--jdk-only`, **UNARMED**, today:

```text
HotSpot   table cls=[Ljava.util.HashMap$Node;  len=16
          bucket[5] cls=java.util.HashMap$Node  isEntry=true
          bucket[6] cls=java.util.HashMap$Node  isEntry=true
CratonVM  table cls=[Ljava.lang.Object;        len=16
          bucket[5] cls=cratonvm.synthetic.AnonymousObject$4  isEntry=false
          bucket[6] cls=cratonvm.synthetic.AnonymousObject$4  isEntry=false
```

**Prediction:** both buckets become `java.util.HashMap$Node`, `isEntry=true`.
`table cls` stays `[Ljava.lang.Object;` — this change does not touch the array
(see §7 N1). **Falsified** by a bucket still reporting `AnonymousObject$4`,
which would mean either the class does not resolve (memo caching a negative in
real-JDK mode) or the guard is rejecting object-shaped payloads.

### 6b. Unarmed corpus — must be verdict-neutral

`--jdk-only` unarmed **105/105**; `SUITE=all` **100/105 with the same five**;
`SUITE=core` **64/65**. Anything new here is a real-node descriptor path this
record did not anticipate, and the first place to look is a caller that put a
primitive through a spelling the guard's two `matches!` do not cover.

**I did not run the sweep.** `.guard-tmp` is a fixed shared path, `H14` measured
two concurrent sweeps moving a result from 83/104 to 102/104, and I cannot
detect another lane's run — so re-measuring a baseline that is already published
(`9eef86699`) was not worth the collision risk. The numbers above are quoted,
not taken.

### 6c. Armed `java/util/HashMap` — improvement, but I will not name a point

Baseline is **83 / 105**. **Prediction: it improves, and the mechanism is
specific.** `H0-6` §7's failure is

```text
java/lang/ArrayStoreException: cratonvm.synthetic.AnonymousObject$4
    at java/util/HashMap.merge(HashMap.java:1372)
    at java/util/HashMap.resize(HashMap.java:719)
```

`resize()` allocates `new Node[newCap]` and stores `oldTab[i]` into it. A real
`HashMap$Node` passes that store check; an `AnonymousObject$4` does not. Same
for every `checkcast Map$Entry` in an `entrySet()` walk. So the vectors whose
armed death is an `ArrayStoreException` or a `Map$Entry` cast should recover.

**The number, since one was asked for: 87 / 105, ARGUED, with a wide band.**
The derivation, so it can be attacked rather than just compared: of `H0-4` §1's
22 `HashMap` failures (23 net of `RMapGcStress`, which `H14-3` re-classified as a
`rc=124` clock artefact), the ones whose armed death is an
`ArrayStoreException` or a `Map$Entry` cast are the ones this reaches.
`H0-6` §7 exhibits exactly one of those by name (`RJdkCollections`). I have no
per-vector failure text for the other 21 — **I did not run them** — so the split
between "dies on the node's class" and "dies on something else" is a guess, and
I am guessing about a fifth: **+4, i.e. 83 → 87.** Anything in **84–92** I would
call the prediction confirmed in kind.

The band is wide because of `H16-3`: the armed state is a MIXED table — the
first put in the process goes to real bytecode and every later one to the
native, in the same map — and three probes over the same operation give three
different armed outcomes (`ClassCastException` after 0; a silent 1-of-2; an NPE
inside `HashMap$EntryIterator.<init>`). A cell measured on that state does not
decompose cleanly, and a point estimate against it is arithmetic this directory
keeps recording as false.

**Falsifiers, in order of what each would mean:**

* **stays exactly 83** → the node class is not what those vectors die on, and
  `H0-4` §7 / `H0-6` §7's mechanism needs re-deriving rather than re-quoting.
* **drops below 83** → the mixed table this leaves is *worse* than the
  uniformly-wrong one. The next move is then to make the guard total (always the
  real class, and repair the primitive producers) rather than to widen it.
* **`RMapGcStress` fails with `rc=124`** → that is the clock, not this change.
  `H14-3` measured it needing 233 s against a 120 s budget. Use `TIMEOUT=600`.

### 6d. Throughput — the objection this change has to answer

`native_map_put_evict_pinned` is the hottest collection path in the tree. The
change adds, per put: two `matches!` on a `Value`, one `vm_identity()` call and
one thread-local `Cell` read; and once per VM per thread, a
`class_id_by_name` + possibly `ensure_class_initialized` + `class_name_of_id`.
**ARGUED to be under noise; NOT MEASURED.** Falsified by an A/B on one binary
showing a map-put-dominated workload (H2's `TestFileSystem`, or `AlCallCostProbe`
against a map) regressing beyond noise. `[MTwall=noise]` applies — this must be
a single-threaded measurement on a quiet host.

## 7. What I did NOT verify

* **The change was never compiled.** `rustfmt --check --edition 2021` parses it
  clean, which is a parse verdict and explicitly not a type check
  (`scripts/merge-parse-check.sh`'s own header says so). A wrong argument count
  or an unresolved name would pass that gate.
* **I did not run any suite** — see §6b.
* **The `table` array's component type is still wrong** and this change does
  not address it: `[Ljava.lang.Object;` where the class declares
  `[Ljava/util/HashMap$Node;`. Real bytecode's `getfield table` survives it only
  because the `b'['` coercion arm passes any `Object` through. Fixing it would
  make `set_array_element` store-check our nodes, which the guard's untyped
  fallback would then fail — so it is a follow-on, not a companion.
* **I did not measure whether any real map in the corpus actually hits the
  guard.** The primitive producers are source-verified; their live frequency is
  not known. If it is zero the guard is free; if it is high the fix is partial
  in a way §6c would show.
* **Compatible (non-`--jdk-only`) mode is untested.** `chm_real_class` resolves
  the same real class there, so the change is NOT `--jdk-only`-scoped and the
  default-mode suites are in scope for it.
* **`synthetic-jdk` was reasoned about, not built.** The mock context's
  `ensure_class_initialized` returns `Ok(ClassId::new(0))` unconditionally and
  its `class_id_by_name` answers only for names the test defined, so the memo
  should cache the sentinel and every in-file mock test should be byte-identical.
  `[2cfgs]` says verify both feature configs; I verified neither.

## 8. NOMINATIONS

* **N1 — make `table` a real `HashMap$Node[]`.** It is the other half of §6a's
  measurement and the reason armed real bytecode is walking an `Object[]` typed
  as a `Node[]`. Needs the guard's untyped fallback removed first, or the array
  store will refuse it.
* **N2 — the four `try_alloc_synthetic(ctx, "java/util/Map$Entry", 3)` sites
  allocate an instance of an INTERFACE.** HotSpot cannot produce such an object.
  Each should become `java/util/AbstractMap$SimpleEntry` (the class the
  measured entrySet path already uses) or the real per-family entry.
* **N3 — `f8aff25bb`'s residual should be re-taken or retired.** Two of its
  candidate mechanisms are dead (§2, §3) and its other half was repaired
  afterwards (§4). If a residual survives this change, it needs a new
  measurement and a new name; it must not be re-quoted from 2026-08-04.
* **N4 — a rule for "necessary but not sufficient" comments.** Carry the DATE
  and the COMMIT of the measurement in the comment itself. This one cost
  seventeen days of not attempting a fix, and the repair for one of its two
  named symptoms landed in this same directory the day before this lane opened.

---

## VERIFIED ON A BUILD (lane H0, 2026-08-21) — and it is a COMPATIBLE-mode win

Built at `757a9cf4f` (`cratonvm-r8.exe`, 0 errors). Same probe as `H16-3`'s
reproduction: three `HashMap`s, three puts each, `HashMap.table` read
reflectively, **UNARMED `--jdk-only`**.

| | before (`r6`) | after (`r8`) | HotSpot |
|---|---|---|---|
| map1 buckets | real 0, fabricated 3 | **real 3, fabricated 0** | real 3 |
| map2 buckets | real 0, fabricated 3 | **real 3, fabricated 0** | real 3 |
| map3 buckets | real 0, fabricated 1 | **real 1, fabricated 0** | real 1 |

**Every node is now a real `java.util.HashMap$Node`, with no dial armed.** This
is a fix in the shipping default configuration, not a strict-mode-only change
and not something that needed the enforcement dial to matter — which is worth
stating because `H16-3` had just shown that the dial measures a hybrid no
retirement reaches. This result does not depend on the dial at all.

### The array component type is still wrong, exactly as this lane nominated

```
tableCls = [Ljava.lang.Object;        CratonVM, after the fix
tableCls = [Ljava.util.HashMap$Node;  HotSpot
```

So a real `Node` now lives inside an `Object[]`. That is the **`new_ref_array`**
half of the sentinel — 29 sites tree-wide (`H0-6` §10) — and this lane said
plainly it was nominating rather than fixing it. Confirmed outstanding.

**Consequence worth being explicit about:** the two halves fail in opposite
directions. A fabricated node in a real `Node[]` throws `ArrayStoreException`; a
real node in an `Object[]` stores fine but leaves `getClass()` on the array
wrong and any code reading the array's component type misled. **Fixing only one
half does not make the pair correct**, and the order matters — node class first
(done here) is the safe order, since the reverse would have produced a real
`Node[]` full of fabrications, which is precisely the armed hybrid.
