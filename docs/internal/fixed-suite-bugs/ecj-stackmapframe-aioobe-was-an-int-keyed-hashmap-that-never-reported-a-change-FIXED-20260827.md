# ecj's `getFramePositions` AIOOBE was an int-keyed `HashMap` that never reported a change

| | |
|---|---|
| **Status** | **FIXED**, 2026-08-27, on `fix/ecj-stackmap-aioobe-20260827` |
| **Symptom** | `ArrayIndexOutOfBoundsException: Index n out of bounds for length n` — always `index == length` — inside `org.eclipse.jdt.internal.compiler.codegen.StackMapFrameCodeStream.getFramePositions` |
| **Blast radius measured** | 70 previously non-passing Tomcat classes: **6 PASS before, 53 PASS after** |
| **Underlying defect** | a `java.util.HashMap` with `Integer` keys reported `modCount == 0` for its entire life |
| **Predecessor** | the OPEN tomcat known-issue page naming this as 49 of 81 non-passing classes, deleted in the same commit that added this record |

## What the exception actually was

`javap -c -l` on the shipped `ecj-4.40.jar` — the version Jasper compiles every
JSP with — puts line 193 inside this loop:

```java
Set  set       = this.framePositions.keySet();   // 188-189
int  size      = set.size();                     // 189
int[] positions = new int[size];                 // 190
int  n         = 0;                              // 191
for (Object pos : set) {                         // 192
    positions[n++] = ((Integer) pos).intValue(); // 193  <-- AIOOBE
}
```

So the exception says exactly one thing: **`set.size()` and `set.iterator()`
disagreed about how many elements the same view had, in the same method, with
nothing in between.** Nothing about ecj, JSPs, Jasper or Tomcat is load-bearing
— they are just the largest caller in the suite.

`framePositions` is a `HashMap<Integer, FramePosition>`. The key type is the
whole story.

## The chain, from the bottom

1. `try_hm_int_fast_put` (`native-collections/src/lib.rs`) keeps an int-keyed
   `HashMap`'s entries in a **Rust side table**, not in the node chain. That is
   what the overlay is for, and `jit_overlay_hashmap_put` reaches it from
   compiled code.

2. It returned without calling `bump_map_mod_count`. So did the overlay
   `remove` and `clear` fast paths. Measured with `probes/MapFieldProbe`:

   | | `puts=6` | after `remove` | after `clear` |
   |---|---:|---:|---:|
   | HotSpot `modCount` | 6 | 7 | 8 |
   | CratonVM `modCount` | **0** | **0** | **0** |

3. The keySet-view rebuild elision (`perf/lazy-map-views-*`) reads that counter
   and **skips a resync while it has not moved**. `view_source_generation`'s
   own doc comment is explicit that the guard "may OVER-invalidate freely; it
   must never UNDER-invalidate" — and a counter frozen at 0 is not a missing
   signal, it is a permanent "nothing has changed".

4. So the view froze at whatever it held when it was built. `probes/MapGenProbe`
   on the unfixed binary:

   ```text
   empty:        map.size=0 keySet.size=0 iterated=0 modCount=0
   after 3 puts: map.size=3 keySet.size=0 iterated=3 modCount=0
   after 6 puts: map.size=6 keySet.size=0 iterated=6 modCount=0
   ```

   `size()` served the frozen backing; the iterator resynced from the source.
   That pair IS the ecj loop, and the same probe reproduces the exception in
   eight lines of Java with no Tomcat, no Jasper and no ecj:

   ```text
   ecj-shape round 0: size=3 iterated=3
   ecj-shape round 1: size=3 AIOOBE Index 3 out of bounds for length 3
   ```

5. A second, independent half: a keySet view's **backing** is reset through its
   FIELDS on every rebuild (`publish_map_table`, `modCount = 0`,
   `set_map_size(.., 0)`), and the integer overlay does not live in those
   fields. `native_map_size` prefers the overlay's count, so the backing
   accumulated every key it had ever shown and `keySet().size()` became a
   high-water mark — still 6 after a `remove` took the map to 5 and a `clear`
   to 0.

## How it was localized

Three steps, no guesswork, roughly forty minutes of machine time:

* **A single-binary A/B on the kill switch the elision already had.**
  `CRATONVM_MAP_VIEW_CACHE=0` turned `jakarta.el.TestCompositeELResolver` from
  `AssertionError: expected:<200> but was:<500>` into `OK (1 test)`, with zero
  `StackMapFrameCodeStream` hits in the log. That named the subsystem before
  anything was read.

* **The verifier the elision already had.**
  `CRATONVM_VERIFY_MAP_VIEW_CACHE=1` takes the fast path's decision, rebuilds
  anyway and compares. It panicked with both key multisets AND the caller:

  ```text
  the generation guard did not move but the contents did.
    elided  (identity hashes): [138909, 138910, 138911]
    rebuilt (identity hashes): [138909, 138910, 138911, 138915, 138916, 138917]
  (native invoked from StackMapFrameCodeStream.getFramePositions()[I)
  ```

* **A trace keyed on object identity, not on class.** Two hypotheses died here
  and both were worth killing on the instrument rather than in prose:

  - *"the generation slot resolves past the object's fields, so the write is
    dropped."* Refuted: `modCount UNMOVABLE` fired 41 times in a run, on
    `cratonvm/synthetic/AnonymousObject$3` — never on a `java/util/HashMap`,
    which reported `nf=18 slot=5` and bumped 2164 times.
  - *"the bump lands somewhere the reader does not look."* Refuted the same
    way: `bump id=1192 … 5->6` and `decide id=1199 … 0->0` are **different
    objects**. The probe's map never appeared in a single `bump` line, while
    `put-in id=1199` appeared for every put. The put entered and left without
    reaching the bump — which is the overlay's early return, and that is the
    defect.

## The fix

`fix(collections): an int-keyed HashMap now reports that it changed` (73f67300f)
plus `fix(jit,collections): a field access with no constant index is refused`
(9198c795f):

* the overlay `put`/`remove`/`clear` move the generation, and only for a
  structural change — a value-replacing put still moves nothing, as on HotSpot;
* `materialize_hm_int_fast` captures the generation and puts it back. Moving
  the overlay into the node table is a representation change Java cannot see,
  and its per-entry re-put would otherwise throw a
  `ConcurrentModificationException` at an iterator already in flight;
* `bump_map_mod_count` resolves the slot bounded by `object_num_fields`, and
  `map_itr_mod_count` now delegates to the same resolver — one implementation,
  memoized per `(vm, class)` so the new bump does not take the class-manager
  read lock on a JIT fast path;
* a keySet view's backing is purged of its own overlay when the view is built
  and on every rebuild.

## What it bought

Same host, same harness, same 70-class list (every class not `PASS` in the
2026-08-24 `sweep4` run), three shards, **arms run sequentially** — a contended
host inverts an A/B:

| arm | PASS | FAIL | HANG |
|---|---:|---:|---:|
| `dev` 48516886c | 6 | 59 | 5 |
| + fix | **53** | 13 | 4 |

48 classes flipped to PASS: the whole `org.apache.jasper.*` family, both
`TestFormAuthenticator*` sets, `TestHttp11Processor`, `TestAbstractAjpProcessor`,
`TestDefaultServlet`, `TestTomcat`, `TestMapperWebapps`, and every `jakarta.el`
/ `jakarta.servlet.jsp` class in the list.

One class flipped the other way, `org.apache.catalina.manager.TestHostManagerWebapp`
(`java.net.SocketTimeoutException: Read timed out`). Re-run 3x on each binary on
a quiet host: `OK (1 test)` 3/3 on both. It is flaky under shard load, not a
regression.

Also green with the fix:

* `regression-suite/run.sh`: **72 passed, 0 failed**;
* `cargo test -p cratonvm-native-collections --lib`: 143 passed, 0 failed
  (four new tests, see below);
* `jakarta.el.TestCompositeELResolver` under `CRATONVM_VERIFY_MAP_VIEW_CACHE=1`:
  `OK (1 test)`, no divergence — i.e. no under-invalidation survives.

## Blast radius beyond Tomcat

The predecessor page asked whether the AIOOBE could silently affect classes
currently counted PASS. The answer is broader than it framed: **this was never
an ecj defect.** Any code that takes `keySet()` on an int-keyed `HashMap` and
then reads `size()` got a stale answer — silently, with no exception, unless it
happened to index an array with it the way ecj does. The `modCount` freeze also
meant `ConcurrentModificationException` could not fire for such a map at all.
ecj is simply the caller that turned a wrong number into a crash instead of into
wrong output.

## Instruments left behind

* `probes/MapGenProbe.java` — `keySet().size()` vs the same view's iterator vs
  `modCount`, plus the ecj loop shape. Diffs against HotSpot line for line.
* `probes/MapFieldProbe.java` — `size`/`modCount`/`threshold` read reflectively
  as one another's controls, so a frozen counter can be told from a broken
  reader.
* `[MAP-VIEW-CACHE] EXIT … modcount_bumped=… modcount_refused=…` on
  `CRATONVM_DBG=map-view-cache`, and a one-line-per-class
  `modCount UNMOVABLE on <class>: num_fields=… resolved_slot=…` naming the
  population that has no generation at all.
* `CRATONVM_VERIFY_MAP_VIEW_CACHE=1` now prints the source class, the generation
  it compared, the generation as of the rebuild and the backing's stamp — so the
  next divergence says which half was wrong rather than only that one was.

## What is worth remembering

A guard whose signal is a counter has two failure modes, and only one of them
looks like a failure. A **missing** counter makes every reader rebuild — slow,
correct, visible in a profile. A **frozen** counter makes every reader skip —
fast, wrong, and invisible until something indexes an array with the answer. The
audit that shipped the elision enumerated which mutators call
`bump_map_mod_count`; it could not have caught this, because the overlay put is
not a mutator that forgot to call it — it is a mutator nobody had listed as one.
