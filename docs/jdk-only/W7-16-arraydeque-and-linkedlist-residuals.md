# W7-16 — `ArrayDeque` streamed empty, and the two gates on `LinkedListSnapshotListItr`

> # A34 2026-08-12 — RUN, in three arms. ALL THREE DEFECTS ARE CLOSED BY
> # MEASUREMENT. Every claim below this banner was source-only until now.
>
> This record, and the reconciliations stacked on top of it, closed three
> defects "in source, NOT REBUILT". A binary that carries all three exists —
> `scratchpad/bin/cratonvm-merged-dev.exe`, built 2026-08-12 15:27 — and the
> whole file has now been re-measured on it against Microsoft JDK 25.0.3.9,
> `--java-home` passed on every CratonVM row, one binary per arm with only the
> mode flag differing. HotSpot 25.0.3.9 is the oracle. **This banner is
> observation. Everything below it about source remains a claim.**
>
> **Part 1, `ArrayDeque` — CLOSED, measured.** Every row of the three-arm table
> further down now reads HotSpot's answer in BOTH CratonVM modes, including the
> four that were the whole defect:
>
> | member (`C1` = `new ArrayDeque<>(List.of("a","b"))`) | HotSpot | `--real-jdk` | `--jdk-only` |
> |---|---|---|---|
> | `LAYOUT` | `len=3 head=0 tail=2` | `len=3 head=0 tail=2` | `len=3 head=0 tail=2` |
> | `toArray(T[])` | `[a, b]` | `[a, b]` | `[a, b]` |
> | `spliterator().estimateSize()` | `2` | `2` | `2` |
> | `stream().count()` | `2` | `2` | `2` |
> | `parallelStream().count()` | `2` | `2` | `2` |
> | `iterator()` / `String.join` | `ab` / `a\|b` | `ab` / `a\|b` | `ab` / `a\|b` |
>
> `C5` (sixteen `addLast` on a default deque — the shape with no collection
> constructor in it) reads `len=17 head=0 tail=16`, `stream().count()=16` and a
> full `toArray(T[])` on all three arms. The spare slot is the mechanism, as
> Part 1 argued, and the falsifying observation it wrote did NOT fire.
>
> **The quieter finding is closed too, and it was worth printing.** After one
> `poll()` the `elements` array reads
> `[null, y, null, …]` on all three arms, and after `clear()` it is all-`null`
> on all three — byte-identical to HotSpot. The dropped references are no
> longer strongly reachable from the buffer.
>
> **Part 2, `LinkedListSnapshotListItr` — CLOSED, measured, both halves live.**
> Under `--jdk-only`: `listIterator()` forward is `abc`, `subList(0,2)` is
> `[a, b]`, and — the row Part 2 called the most important in its table —
> `arrayList.equals(linkedList)` is **`true`**, where it answered a laundered
> `false` with no exception before. The `UnsatisfiedLinkError`-at-`hasNext()`
> failure mode that "land both hunks or neither" was written to prevent did not
> occur: the retag and the mint are both in, and the nine natives dispatch.
>
> **Part 3, the `jdk_interfaces` arm — CLOSED, measured, and this is the first
> time `probes/ListItrInterfaceProbe.java` has ever been executed on CratonVM.**
> `SUMMARY pass=16 fail=0` in **both** modes, matching
> `ListItrInterfaceProbe.expected.txt`'s stated AFTER state exactly:
>
> ```text
> ROW ll.interfaces [java.util.Iterator, java.util.ListIterator]
> ROW ll.instanceofListIterator true  want=true  PASS
> ROW ll.instanceofIterator     true  want=true  PASS
> ROW ll.castListIterator       ok:true          PASS
> ROW ll.castIterator           ok:true          PASS
> ROW ll.idx.castListIterator   ok:true          PASS
> SUMMARY pass=16 fail=0
> ```
>
> The instrument's own calibrations held on the CratonVM arms, which is what
> makes the greens mean anything: `selfTestNoCheckcast` passed, all three
> `selfTestRed*` rows produced their expected `ClassCastException`/`false`, and
> the entire `al.*` control family passed. The expected transcript's single
> falsifying observation — `castIterator` passing while `castListIterator`
> raises — did not fire, so assignability IS walking super-interfaces
> transitively.
>
> The two unscored INFO rows behave as designed and are worth reading as
> provenance: `ll.getClass` is `cratonvm.internal.LinkedListSnapshotListItr` in
> both modes, and `ll.iterator.getClass` splits by mode —
> `java.util.LinkedList$Itr` under `--real-jdk`, `java.util.Arrays$ArrayItr`
> under `--jdk-only`. That second row is W2-1's strict iterator fallback
> observed working, and it dates this binary as post-`6ae3ca634`.
>
> **Scheduling — unchanged, and still the real gap.** `regression-suite/run.sh`
> names no path under `probes/` at any `SUITE=` value (re-grepped: the string
> `probes` does not appear in it). The three closures above are therefore
> **discharged but unscheduled** — nothing in CI re-runs them, and the two
> fixtures that touch `listIterator` still assign through the declared return
> type, so javac emits no `checkcast` and they cannot see Part 3 even in
> principle. The nomination for an erased-type vector below stands unchanged;
> what has changed is that it would now be pinning a green rather than chasing
> a red.
>
> **NEW, and not previously probed by anyone: `--synthetic-jdk` RUNTIME MODE.**
> A `--features synthetic-jdk` binary now exists
> (`C:/craton/synjdk-target/release/cratonvm.exe`), so the mode this record's
> two families were never measured in is reachable for the first time. Three
> findings, all measured, none of them this record's fault and all of them in
> its subject matter — see *"A34 2026-08-12 — the `--synthetic-jdk` arm"* at
> the end of this file. The one that matters is not a refusal:
> **`linkedList.equals(arrayList)` answers `false` while
> `arrayList.equals(linkedList)` answers `true`** on the same pair, which is a
> silent wrong answer and an asymmetric `List.equals`.

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md) — THE
> `LinkedListSnapshotListItr` HALF WAS TAKEN.** Both hunks of the "not applied"
> patch are in the tree, landed together as this record required ("one commit
> or neither") by the W7-20 lane, commit `6ae3ca634`: (a) the retag —
> `"cratonvm/internal/LinkedListSnapshotListItr"` is now in
> `VM_SERVICE_RECEIVERS` at `native-api/src/no_image_receiver.rs:267`, with the
> tombstone at `:198`; (b) the VM-internal mint —
> `ensure_vm_internal_class(..., 3)` at `native-collections/src/lib.rs:31049`
> and `:31076`.
>
> * **`ArrayDeque`: CLOSED in source.** `ad_ensure_capacity` at
>   `native-collections/src/lib.rs:34020`, commit `fddf67650`.
> * **Residual: CLOSED IN SOURCE 2026-08-12 (W7-62-ratchets-and-dead-code.md),
>   NOT REBUILT.** The third defect — the `jdk_interfaces` arm — is applied,
>   between the `ArrayListSubList` and `java/util/Dictionary` entries this
>   record's diagnosis named. The mechanism was checked before the edit rather
>   than assumed: `jdk_interfaces` is read by `fabricate_class`, which is the
>   shared body of ALL THREE `ensure_*_class` entry points, so it reaches the
>   `ClassOrigin::VmInternal` door the carrier is minted through since
>   `6ae3ca634` and not only the compatibility door the record was written
>   against. `is_synthetic_collection_iterator` does not match this name, so
>   the table was in fact the carrier's ONLY source of interfaces.
>
>   It **closes** the ClassCastException rather than moving it:
>   `Class::is_assignable_to_name_inner` walks interfaces transitively, the
>   nine `ListIterator` methods are exactly the nine natives registered on the
>   carrier, and dispatch probes the registry from the receiver's own class
>   name first — so `invokeinterface` through the new declaration lands on the
>   same bodies. What it does NOT close is the thing behind the carrier:
>   `listIterator().getClass()` still answers
>   `cratonvm.internal.LinkedListSnapshotListItr` (deliberately — see Part 3 of
>   W7-20-refusal-laundered-into-wrong-answer.md), and a real `ListItr.remove()`
>   against a native `LinkedList` still leaves `size` stale. Those are a
>   collections reclassification, not an interface list.
>
>   The instrument is `probes/ListItrInterfaceProbe.java`, with a HotSpot 25
>   control transcript in `probes/ListItrInterfaceProbe.expected.txt` and the
>   before/after CratonVM rows stated there. It carries a red calibration (three
>   rows whose expected answer is the ClassCastException) because every other
>   row passes when a cast SUCCEEDS.
> * **ADJUDICATED 2026-08-12 — the source claim above is CORRECT, re-verified
>   from the tree rather than from W7-62's word, and the run that closes it is
>   NOT a suite run.**
>   * The `jdk_interfaces` arm is present at
>     `classloading/src/class_manager.rs:10979` and reads
>     `&["java/util/ListIterator", "java/util/Iterator"]`, sitting between the
>     `cratonvm/internal/ArrayListSubList` entry (`:10931`) and
>     `"java/util/Dictionary"` (`:10982`) — exactly the position this record's
>     out-of-file patch named.
>   * The mechanism claim holds and is in fact wider than stated:
>     `jdk_interfaces` is read at **two** sites, `fabricate_class`
>     (`:3757`, read at `:3977`) and `create_synthetic_stub` (`:8963`, read at
>     `:9023`), so the arm reaches the `ClassOrigin::VmInternal` door **and**
>     the compatibility-stub door.
>   * The paired halves are in the tree: the retag at
>     `native-api/src/no_image_receiver.rs:267` with the tombstone at `:198`,
>     and the mint through `ensure_vm_internal_class(..., 3)` in both
>     `native_ll_list_iterator` and `native_ll_list_iterator_idx`. **Those two
>     line numbers have DRIFTED** — they are `native-collections/src/lib.rs:32001`
>     and `:32028` today, not `:31049`/`:31076`; anchor on the function names.
>   * **What closes it.** `probes/ListItrInterfaceProbe.java` and
>     `probes/ListItrInterfaceProbe.expected.txt` both exist, and
>     `regression-suite/run.sh` names **no path under `probes/` at any `SUITE=`
>     value** — grepped. So this record cannot be discharged by a suite run,
>     however green, and the entry in README §2.6 is the right home for it. The
>     only two fixtures that touch `listIterator` — `RJdkCollections.java:66`
>     and `RJdkViews.java:209` — assign it straight into a declared
>     `ListIterator<String>`, so javac emits no `checkcast`, which is the exact
>     reason this record gives for the family working at all. **They could not
>     see this defect even in principle.** A scheduled vector would have to
>     erase the type — `Object o = ll.listIterator();` then
>     `o instanceof ListIterator` and `(ListIterator<?>) o` — in a fixture
>     `run.sh` runs, in both arms.
> * **Also open, and PARTLY settled:** W7-20's two frozen baselines. The
>   kind-map one is re-frozen (twelve rows, by hand, disclosed in its header);
>   the bridge-ratchet JSON needs a census. See
>   W7-20-refusal-laundered-into-wrong-answer.md and
>   W7-62-ratchets-and-dead-code.md.

**Status: `ArrayDeque` DIAGNOSED and FIXED IN SOURCE 2026-08-11, NOT REBUILT.
`LinkedListSnapshotListItr` DIAGNOSED, deliberately NOT changed — the
sufficient patch spans two files and only one of them is this branch's.**

Every measurement below was taken by running the already-built binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` against Temurin
`jdk-25.0.3.9-hotspot` on windows/x64, one binary per row with only the mode
flag differing. Nothing here claims a source change works; the "before" rows
are observations and the "after" rows are claims about source.

Branch: `fix/collections-arraydeque-and-linkedlist-residuals-20260811`.
File changed: `native-collections/src/lib.rs`, and nothing else.

Predecessors: `W2-1-strict-refuses-the-synthetic-stream-stack.md` (which left
both of these open, and whose `ArrayDeque` diagnosis is corrected here),
`W7-13-strict-mh-insert-wrapper.md` (the `VmInternal` door),
`W7-1-treemap-views-and-iterator-remove-contract.md` (why the probe prints
content).

---

## Part 1 — `ArrayDeque` streamed empty, and it was never the unwritten `tail`

### The reported defect

`W2-1` residual 2: `ArrayDeque.stream().count()` answers `0` where HotSpot
answers `2`, in **both** compatibility modes, with `size()` / `toString()` /
`contains()` correct beside it. Root cause given there: *"the unwritten `tail`"*
— a reflective read of a two-element deque showing `elements=Object[2] head=0
tail=0`.

The observation reproduces exactly. The diagnosis under it does not survive one
control.

### The control that names the real defect

The reflective read was taken from `new ArrayDeque<>(List.of("a","b"))`. Build
the *same two-element deque* with the no-arg constructor instead and nothing is
wrong at all:

| how the deque was built | CratonVM layout | `stream().count()` |
|---|---|---|
| `new ArrayDeque<>(List.of("a","b"))` | `elements.len=2 head=0 tail=0` | **0** |
| `new ArrayDeque<>(); addLast("a"); addLast("b")` | `elements.len=16 head=0 **tail=2**` | **2** |

`tail` is written, by `native_ad_add_last`, on every call. What the first row
shows is not an unwritten field — it is a **wrap**. Two elements exactly filled
a two-slot buffer, so `tail` advanced `0 → 1 → 0` and landed back on `head`.

### Why a full buffer reads as an empty deque

Slots 0..2 of CratonVM's `ArrayDeque` overlay are not CratonVM's. `javap -p
java.util.ArrayDeque` on Temurin 25.0.3 gives three instance fields in
declaration order:

```text
transient java.lang.Object[] elements;
transient int head;
transient int tail;
```

which is `AD_FIELD_DATA` / `AD_FIELD_HEAD` / `AD_FIELD_TAIL`. Only `size` at
slot 3 is ours, and it exists because `synthetic_stub_fields` pads
`java/util/ArrayDeque` to four instance fields (`instance_fields(4)` in
`classloading/src/class_manager.rs`).

So the deque has **two owners of its length**: our slot-3 `size`, and the
`head`/`tail` pair that every piece of real `ArrayDeque` bytecode uses. The real
one is computed, not stored —

```java
public int size() { return sub(tail, head, elements.length); }
```

— and that arithmetic cannot distinguish *full* from *empty*. The JDK resolves
the ambiguity by **never letting the buffer fill**:

```java
public ArrayDeque()            { elements = new Object[16 + 1]; }
public ArrayDeque(int n)       { elements = new Object[(n < 1) ? 1
                                          : (n == Integer.MAX_VALUE) ? Integer.MAX_VALUE
                                          : n + 1]; }
```

Confirmed by measurement, not only by reading: HotSpot prints `elements.len=17`
for a default deque and `elements.len=3` for `new ArrayDeque<>(2)`. With the
spare slot, `head == tail` means empty and only empty.

CratonVM allocated exactly `16` and exactly `max(n, 1)`, and `ad_ensure_capacity`
returned early on `min_cap <= old_cap` — i.e. it grew only when the buffer was
*over*full. Every deque whose element count landed exactly on its capacity
therefore sat in the one state the real layout reads as empty.

`new ArrayDeque<>(Collection)` hits it every time, because its real body is
`this(c.size()); copyElements(c);` and `ArrayDeque(int)` is one of ours.

### The three-arm table (deliverable)

`--add-opens java.base/java.util=ALL-UNNAMED` throughout, so the `LAYOUT` row is
a real reflective read on all three arms. Every row prints **content**.

`C1` is `new ArrayDeque<>(List.of("a","b"))` — the failing shape.

| member | HotSpot 25 | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| **`LAYOUT`** | `len=3 [a, b, null] head=0 tail=2` | **`len=2 [a, b] head=0 tail=0`** | **`len=2 [a, b] head=0 tail=0`** |
| `size()` | `2` | `2` | `2` |
| `isEmpty()` | `false` | `false` | `false` |
| `toString()` | `[a, b]` | `[a, b]` | `[a, b]` |
| `toArray()` | `[a, b]` | `[a, b]` | `[a, b]` |
| **`toArray(T[])`** | `[a, b]` | **`[]`** | **`[]`** |
| `iterator()` | `[a, b]` | `[a, b]` | *NoClassDefFoundError: `java/util/ArrayDeque$Itr`* |
| `descendingIterator()` | `[b, a]` | `[b, a]` | `[b, a]` |
| **`spliterator().estimateSize()`** | `2` | **`0`** | **`0`** |
| **`spliterator()` drain** | `[a, b]` | **`[]`** | **`[]`** |
| `forEach()` | `[a, b]` | `[a, b]` | `[a, b]` |
| **`stream().count()`** | `2` | **`0`** | **`0`** |
| **`stream().toList()`** | `[a, b]` | **`[]`** | **`[]`** |
| **`parallelStream().count()`** | `2` | **`0`** | **`0`** |
| **`parallelStream().toList()`** | `[a, b]` | **`[]`** | **`[]`** |
| `contains("a")` | `true` | `true` | `true` |
| `contains("zz")` | `false` | `false` | `false` |
| `peek()` / `peekFirst()` | `a` | `a` | `a` |
| `peekLast()` | `b` | `b` | `b` |
| `element()` / `getFirst()` | `a` | `a` | `a` |
| `getLast()` | `b` | `b` | `b` |
| `new ArrayList<>(d)` | `[a, b]` | `[a, b]` | `[a, b]` |
| `String.join("|", d)` | `a\|b` | `a\|b` | *NoClassDefFoundError: `java/util/ArrayDeque$Itr`* |
| `clone().toString()` | `[a, b]` | `[a, b]` | `[a, b]` |

**The split is exact and it is the whole diagnosis.** Every member CratonVM
registers a native for is correct, because those read slot 3. Every member that
is real bytecode over `head`/`tail` is wrong: `stream`, `parallelStream`,
`spliterator`, and `toArray(T[])` — which we do not register, so it is the real
`ArrayDeque.toArray(T[])`.

`--real-jdk` and `--jdk-only` differ on nothing here except
`java/util/ArrayDeque$Itr`, which is `W2-1`'s already-fixed-in-source
iterator gap and is absent from this pre-built binary (see *Binary provenance*
below). **Defect 1 is a `Compatible` defect**, exactly as reported.

### The control row, and the case that makes it worse

Same probe, `C2` = `new ArrayDeque<>(); addLast("a"); addLast("b")`:

| member | HotSpot 25 | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| `LAYOUT` | `len=17 … head=0 tail=2` | `len=16 … head=0 tail=2` | `len=16 … head=0 tail=2` |
| `toArray(T[])` | `[a, b]` | `[a, b]` | `[a, b]` |
| `spliterator().estimateSize()` | `2` | `2` | `2` |
| `stream().count()` | `2` | `2` | `2` |
| `parallelStream().toList()` | `[a, b]` | `[a, b]` | `[a, b]` |

Every member identical to HotSpot. A capacity that is merely *different* costs
nothing; a capacity the count *reaches* costs everything. That is why this
survived — and why it is not a rare shape:

`C5` = sixteen `addLast` on a default deque, i.e. an ordinary deque that simply
grew into its own capacity:

| member | HotSpot 25 | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| `LAYOUT` | `len=17 head=0 tail=16` | `len=16 head=0 **tail=0**` | `len=16 head=0 **tail=0**` |
| `size()` | `16` | `16` | `16` |
| `toString()` | `[e0 … e15]` | `[e0 … e15]` | `[e0 … e15]` |
| `stream().count()` | `16` | **`0`** | **`0`** |
| `toArray(T[])` | `[e0 … e15]` | **`[]`** | **`[]`** |
| `spliterator()` drain | `[e0 … e15]` | **`[]`** | **`[]`** |

No collection constructor involved. Any deque that reaches 16, 32, 64 … elements
streams empty until the next element pushes it past the boundary.

`D5` (`addAll` into a 4-slot buffer, reaching 4) is the same shape and also
answered `stream().count() = 0` against HotSpot's `4`.

### The fix

`native-collections/src/lib.rs`, three sources of the same invariant:

1. **`ad_ensure_capacity`** — `if min_cap < old_cap { return; }` (was `<=`) and
   `new_cap = max(old_cap * 2, min_cap + 1)` (was `min_cap`). The buffer is now
   always strictly larger than the element count it must hold.
2. **`native_ad_init`** — allocates `AD_DEFAULT_CAPACITY + 1`.
3. **`native_ad_init_capacity`** — transcribes the real `ArrayDeque(int)`
   expression, including its `Integer.MAX_VALUE` arm, rather than
   `max(n, 1)`.

Both call sites of `ad_ensure_capacity` (`native_ad_add_first`,
`native_ad_add_last`) pass `size + 1` and are unchanged; `native_ad_add_all`,
`offer*`, `push` and `add` all route through those two and inherit the fix.

**Claimed effect, not verified — nothing was rebuilt.** With the invariant
restored, `head == tail` can only mean empty, so the four real-bytecode members
above should answer as HotSpot does in both modes. The falsifying observation is
below.

### A second, quieter thing the same table shows

HotSpot after one `poll()` on the `C1` deque: `elements=[null, b, null]`.
CratonVM: `elements=[a, b]`. `ad_remove_at_logical` already nulled the vacated
slot for interior removals and says why; `removeFirst`, `removeLast` and
`clear` did not. Nothing reads outside `head..head+size`, so no accessor could
see it — a polled or cleared deque simply kept every dropped element strongly
reachable from its own buffer. `clear()` is the call a caller makes
*specifically* to drop references, so it is the worst of the three. All three
now null what they vacate, mirroring the real `pollFirst`'s `es[h] = null` and
`clear`'s `circularClear(elements, head, tail)`.

### Which mode this changes

**`Compatible`, deliberately.** `stream().count()` on an exactly-full deque goes
`0 → n`; `toArray(T[])`, `spliterator()` and `parallelStream()` likewise;
`elements.length` goes `n → n + 1` for anything reading it reflectively; and the
`elements` array of a polled/cleared deque now holds `null` where it held a
stale reference. `--jdk-only` gets the identical change — the defect was never
mode-dependent. Answering `0` where HotSpot answers `2` is a defect, not a
compatibility guarantee.

### Falsifying observation

Rebuild and re-run the probe. If `C1`/`C5`/`D5` still answer `stream().count() =
0` while `LAYOUT` now reads `len=3 … tail=2` / `len=17 … tail=16`, then the
spare slot was not the mechanism and something else reads slot 3 — re-check
whether `collect_collection_elements`'s `ArrayDeque` arm
(`object_num_fields(coll) > AD_FIELD_SIZE`) is the path being taken instead of
real bytecode. If instead the `size()`/`toString()`/`getFirst()` rows go wrong
while the stream rows go right, the two owners have swapped places: a call site
is now sizing from `head`/`tail` where it used to size from slot 3.

---

## Part 2 — `LinkedListSnapshotListItr`: the `VmInternal` door applies, and is not enough

### The failure, re-measured

`--jdk-only`, three-element `LinkedList`, against `Compatible` on the same
binary:

| member | HotSpot 25 | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| `listIterator().getClass()` | `java.util.LinkedList$ListItr` | `cratonvm.internal.LinkedListSnapshotListItr` | *NoClassDefFoundError: `cratonvm/internal/LinkedListSnapshotListItr`* |
| `listIterator()` forward | `abc` | `abc` | *NoClassDefFoundError* |
| `listIterator(3)` backward | `cba` | `cba` | *NoClassDefFoundError* |
| `nextIndex`/`previousIndex` | `0/-1 then 1/0` | `0/-1 then 1/0` | *NoClassDefFoundError* |
| `listIterator().set("Z")` | `[Z, b, c]` | `[Z, b, c]` | *NoClassDefFoundError* |
| `ll.equals(arrayList)` | `true` | `true` | *NoClassDefFoundError* |
| **`arrayList.equals(ll)`** | `true` | `true` | **`false`** — no exception |
| `subList(0,2)` | `[a, b]` | `[a, b]` | *NoClassDefFoundError* |
| `sort(naturalOrder)` | `[a, b, c]` | `[a, b, c]` | *NoClassDefFoundError* |
| `indexOf("b")` | `1` | `1` | `1` |
| `lastIndexOf("c")` | `2` | `2` | `2` |
| `toString()` | `[a, b, c]` | `[a, b, c]` | `[a, b, c]` |
| `stream().count()` | `3` | `3` | `3` |

So it is strict-only, as expected — with one exception that is the most
important row in the table, below.

### The `VmInternal` door — checked first, as asked

`W7-13` moved ten `MethodHandles` combinator carriers from
`try_alloc_concurrent_synthetic` (`ClassOrigin::CompatibilityStub`, which strict
forbids) to `ensure_vm_internal_class` (`ClassOrigin::VmInternal`, which
contract §1 item 6 permits in both modes), on the test *"does the JVM
specification say a class file must exist for this name?"*

`cratonvm/internal/LinkedListSnapshotListItr` passes that test on every point:

* no JDK image declares a `cratonvm/…` name, and none ever will;
* it is a 3-slot tuple of the VM's own state — `Object[]` snapshot at 0, `Int`
  cursor at 1, backing list ref at 2 — written by `native_ll_list_iterator` and
  read by the nine natives registered on it;
* it is **not** a stand-in for `java/util/LinkedList$ListItr`. The registrar
  comment records that it was deliberately named out of the `java/util/*`
  namespace precisely so it would *not* pick up the real class's 5-field layout,
  which mangled the `Int` cursor write into the real `next:Node` slot and made
  `next()` never advance;
* `fabricated_origin_for_name` stamps it `CompatibilityStub` only by falling off
  the end of four pattern tests — by default, not by a judgement about this
  class;
* and both doors funnel into the same `ClassManager::fabricate_class` with the
  same `num_fields`. Only `origin` and `enforce` differ, so the minted class is
  structurally identical either way. The door changes the policy verdict and
  nothing else.

**And it is still not sufficient.** Two independent gates refuse this carrier,
and the door touches only one:

| gate | where | what it does | cleared by the door? |
|---|---|---|---|
| the **class** | `try_alloc_synthetic` → `fabricate_class`, `enforce = true` | `CompatibilityStub` under `JdkOnly` ⇒ `NoClassDefFoundError` | **yes** |
| its **natives** | `NativeMethodRegistry::register`, `native-api/src/registry.rs` | the name is on `VM_MINTED_STAND_IN_RECEIVERS`, so `receiver_declared_by_no_supported_image` re-tags all nine from the ambient `Bridge` to `SyntheticStub` **before** the `JdkOnly` arm reads the kind — and that arm `return`s without inserting them | **no** |

Measured, not inferred. `--dump-native-registry` on the pre-built binary:

```text
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr hasNext ()Z
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr next ()Ljava/lang/Object;
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr hasPrevious ()Z
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr previous ()Ljava/lang/Object;
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr nextIndex ()I
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr previousIndex ()I
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr set (Ljava/lang/Object;)V
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr remove ()V
synthetic-stub | cratonvm/internal/LinkedListSnapshotListItr add (Ljava/lang/Object;)V
```

`register_linked_list_natives` sets `NativeKind::Bridge` for its whole body and
does not restore the previous category until after these nine — so `Bridge` is
what the registration site asks for, and `synthetic-stub` is what the table
overrides it to. This is the "`NativeKind` is ambient" hazard arriving from a
third direction: not a category left unset, not an unstated re-registration, but
a **central, name-keyed re-tag applied after the site had its say**.

Clearing gate 1 without gate 2 therefore leaves `--jdk-only` holding a
well-formed carrier with no implementation. The failure moves from
`NoClassDefFoundError` *at `listIterator()`* to `UnsatisfiedLinkError` *at the
first `hasNext()`* — later, further from the cause, and no better for the
contract. That is exactly the trap `STRICT_STILL_FABRICATES` is kept as an
**empty** table to name:

> the two halves of this defect are the registration's kind and the class's
> existence, and moving the first without the second turns a silent §5 violation
> into an `UnsatisfiedLinkError`

running in the other direction. **So this branch changed nothing at the mint
site.** Both halves are written out below and must land in one commit; if only
one can land, land neither.

### Why the carrier has to keep existing at all

The alternative — drop the interception and let real `LinkedList$ListItr`
bytecode run — was re-measured, because `W2-1` had already retired two stale
registrar comments claiming the real fields were wrong. They *are* right:
reflectively, a CratonVM `LinkedList` reads `size=3` with `first` holding a real
`LinkedList$Node`, and `descendingIterator()` — which nothing intercepts —
runs real `ListItr` bytecode and answers `cba` under `--jdk-only`.

But driving a real `ListItr.remove()` at a native `LinkedList` still splits the
two owners, and this reproduces in **`Compatible`** mode on the unmodified
binary:

```text
HotSpot   first=c removed; ba  size=2  list=[a, b]
CratonVM  first=c removed; ba  size=3  list=[a, b]
```

Real `unlink` re-linked the real node chain correctly — `list=[a, b]` — and
decremented the real `size`. `ll_get` consults the **overlay** first and still
answers `3`. One owner of the state is the whole reason for the snapshot; the
carrier is how the snapshot is held.

(`W2-1` recorded a harder version of the same split, an `NPE: Cannot read field
"item"` from a subsequent `previous()`. That variant did not fire in this run;
the size disagreement did. Same defect, different depth.)

### The row that makes the current refusal worse than "a loud failure"

`arrayList.equals(linkedList)` answers **`false`** under `--jdk-only`, with no
exception. HotSpot and `Compatible` answer `true`.

The launderer is ours and is in `native-collections/src/lib.rs`.
`native_al_equals`'s cross-layout arm calls `collection_elements_generic`, whose
signature is:

```rust
fn collection_elements_generic(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<Value>
```

No error channel. Its `invoke_virtual(this, "iterator", …)` maps **every**
failure — including the strict `NoClassDefFoundError: java/util/LinkedList$Itr`
— to `Vec::new()`, and `native_al_equals` then compares three elements against
zero.

This is the one failure shape contract §5 cannot tolerate, and it is invisible
from the census: the violation *is* recorded, so the run looks measured, while
the program is handed a wrong answer. **A refusal that reaches a caller with
nowhere to put it stops being a refusal.** Documented at the function, not only
here. Not fixed: every caller of the helper would need an error channel, and
this branch cannot rebuild to verify one.

### The census already names this gap without the carrier

The objection `W2-1` raised against the `VmInternal` door for
`cratonvm/stream/LazyOp` — that minting it *"would make the synthetic pipeline
work under `--jdk-only` and take the gap off the census in the same change"* —
does not carry over, and `--jdk-only-report` says so in its own words. The same
run records this gap three times:

```json
{"kind":"compatibility-class-requested",
 "class":"cratonvm/internal/LinkedListSnapshotListItr", …}

{"kind":"native-shadows-bytecode",
 "summary":"bridge-ran-over-bytecode native shadows bytecode of
            java/util/LinkedList.listIterator()Ljava/util/ListIterator;",
 "class":"java/util/LinkedList","method":"listIterator", …}

{"kind":"synthetic-native-registered",
 "class":"cratonvm/internal/LinkedListSnapshotListItr","method":"hasNext", …}
```

Only the first disappears. The **second** is the row that actually names the
defect — CratonVM serves `java/util/LinkedList.listIterator` from a native
instead of running the JDK's bytecode — and it is keyed on `java/util/LinkedList`
and the registration, not on the carrier, so no change to the carrier can move
it. It is also a `java/util/*` name, which is the brief's point: *an inventory
scoped to `cratonvm/*` under-reports these gaps.* `W2-1`'s own inventory table
is scoped that way and lists this gap under the carrier's name rather than under
`LinkedList.listIterator`, which is how it came to look like an iterator problem
rather than an ownership one.

The distinction from `LazyOp` is therefore not about the census at all — it is
that `LazyOp` had a **correct alternative already wired** (the eager pipeline,
identical element results), so taking the door there would have chosen a
fabrication over a working real-JDK path. `LinkedListSnapshotListItr` has no
correct alternative: real `ListItr` desyncs the overlay in `Compatible` mode
(measured above), a real `Arrays$ArrayItr` has no
`hasPrevious`/`previous`/`set`/`add`/`nextIndex`, and a real `ArrayList$ListItr`
over a snapshot `ArrayList` would write `set()` into the snapshot's
`elementData` and silently not reach the `LinkedList`.

### Out-of-file patch (not applied)

Both hunks, verbatim. **One commit, or neither.**

**(a) `native-api/src/no_image_receiver.rs`** — move the name from
`VM_MINTED_STAND_IN_RECEIVERS` to `VM_SERVICE_RECEIVERS`, so the nine
registrations keep the `Bridge` the registration site already asks for. Delete
this line from `VM_MINTED_STAND_IN_RECEIVERS`:

```rust
    "cratonvm/internal/LinkedListSnapshotListItr",
```

and add it to `VM_SERVICE_RECEIVERS`, which must stay sorted
(`table_is_sorted_and_unique` binary-searches both):

```rust
pub const VM_SERVICE_RECEIVERS: &[&str] = &[
    "cratonvm/Instrument",
    "cratonvm/Util",
    "cratonvm/Wp71JdbcSpi",
    // A `ListIterator` over an immutable snapshot of a native `LinkedList`, and
    // the state that iteration needs: `Object[]` snapshot at slot 0, `Int`
    // cursor at 1, backing list at 2. Contract §11's "reviewed VM service", and
    // `Bridge` is the only tag that survives `allowed_in(JdkOnly)`.
    //
    // It is here rather than in `VM_MINTED_STAND_IN_RECEIVERS` because it
    // stands in for nobody. It was deliberately NOT named
    // `java/util/LinkedList$ListItr` — that name resolves to the real 5-field
    // class, whose layout mangled the cursor write into the real `next:Node`
    // slot and made `next()` never advance, so `AbstractList.equals` compared
    // element 0 forever. The `cratonvm/` name is what keeps the layout ours.
    //
    // Retiring this entry means making the natives stop owning `LinkedList`
    // state, then dropping the `listIterator` interception — a collections
    // reclassification, not an iterator change. Until then, re-tagging it
    // `SyntheticStub` removes `listIterator()`, `subList`, `sort` and
    // `AbstractList.equals`/`hashCode` from `--jdk-only` rather than
    // reclassifying them.
    "cratonvm/internal/LinkedListSnapshotListItr",
    "cratonvm/internal/SystemLogger",
    "cratonvm/test/Util",
    "cratonvm/tls/T27SelfTest",
    "java/lang/reflect/Proxy$Dispatch",
    "java/lang/reflect/Proxy$Instance",
];
```

**(b) `native-collections/src/lib.rs`** — mint through the `VmInternal` door.
Both `native_ll_list_iterator` and `native_ll_list_iterator_idx` carry the same
allocation; replace it in each:

```rust
    // WAS: try_alloc_synthetic(ctx, "cratonvm/internal/LinkedListSnapshotListItr", 3)
    //
    // This carrier is VM-internal state, not a compatibility stand-in: no image
    // declares a `cratonvm/…` name, nothing is being stood in for, and the
    // three slots are the snapshot, the cursor and the backing list. Contract
    // §1 item 6 permits it in both modes, which is the door
    // `W7-13-strict-mh-insert-wrapper.md` established for the ten
    // `MethodHandles` combinator carriers.
    //
    // Load-bearing pairing: this is inert without the companion move of this
    // class from `VM_MINTED_STAND_IN_RECEIVERS` to `VM_SERVICE_RECEIVERS` in
    // native-api/src/no_image_receiver.rs. Without it, `register()` re-tags the
    // nine natives below `SyntheticStub` and `--jdk-only` drops them, so this
    // mint hands strict a well-formed object with no methods and the failure
    // becomes an `UnsatisfiedLinkError` at the first `hasNext()`.
    let it = rooted_across(ctx, &mut [&mut this, &mut arr], |ctx| {
        ctx.ensure_vm_internal_class("cratonvm/internal/LinkedListSnapshotListItr", 3)
    });
    let it = ctx.alloc_object(it, 3);
```

(`ensure_vm_internal_class` returns a `ClassId` and is infallible, so the `?` on
the old `try_alloc_synthetic` goes away and the allocation becomes explicit.
`rooted_across` is still required: `ensure_vm_internal_class` can load a class
and therefore collect.)

**Verification once both land** (neither is verified — nothing was rebuilt):

```sh
cargo build --release -p cratonvm-cli
for M in "--jdk-only" ""; do
  target/release/cratonvm $M --java-home "$JDK" \
    --add-opens java.base/java.util=ALL-UNNAMED -cp probes LLProbe
done
```

Expected: the two arms identical, and identical to `java`, on every row except
`listIterator().getClass()`. The `--jdk-only` run must **still** report
`native-shadows-bytecode` for `java/util/LinkedList.listIterator` under
`--jdk-only-report` — that is the row that keeps the real gap on the census, and
if it disappears the retag went further than intended.

### A third defect the same probe found, also out-of-file

`cratonvm/internal/LinkedListSnapshotListItr` has **no `jdk_interfaces` arm**, so
it implements nothing — in `Compatible` mode, on the current binary:

| expression | HotSpot 25 | `--real-jdk` |
|---|---|---|
| `listIterator() instanceof ListIterator` | `true` | **`false`** |
| `listIterator() instanceof Iterator` | `true` | **`false`** |
| `(ListIterator) someObject` | ok | **`ClassCastException: cratonvm.internal.LinkedListSnapshotListItr cannot be cast to java.util.ListIterator`** |

`AbstractList.equals` never trips this because its receiver is already typed
`ListIterator`, so no `checkcast` is emitted — which is why the family works at
all. Any caller that erases the type does trip it. This is the same trap
`cratonvm/synthetic/Process` was fixed for, and the fix is one line in
`classloading/src/class_manager.rs`'s `jdk_interfaces`, beside the
`ArrayListSubList` entry that is already there:

```rust
        "cratonvm/internal/ArrayListSubList" => &["java/util/List", "java/util/RandomAccess"],
        // Same reason as the entry above and as `cratonvm/synthetic/Process`:
        // without this the object `linkedList.listIterator()` hands back is
        // `instanceof ListIterator == false`, and every erased-type
        // `(ListIterator) x` raises ClassCastException. Recorded here rather
        // than as a `superclass` link because `java.util.ListIterator` is an
        // interface with no fields, so there is no layout to alias.
        "cratonvm/internal/LinkedListSnapshotListItr" => {
            &["java/util/ListIterator", "java/util/Iterator"]
        }
```

Not this branch's file. Independent of both halves above — it is a `Compatible`
defect and lands on its own.

---

## Binary provenance

The pre-built binary used for every measurement predates `W2-1`'s
iterator fixes: it still raises `NoClassDefFoundError` for
`java/util/ArrayDeque$Itr` and `java/util/LinkedList$Itr` under `--jdk-only`,
which that record fixed in source on 2026-08-11. Those two names appear in the
strict columns above and are **not** findings of this record — they are the
known, already-fixed-in-source gap showing through an old binary.

The check that establishes this rather than assuming it: the `Bridge` category
those registrars set landed in `21d47faf5` (2026-06-02), long before the binary,
yet `--dump-native-registry` reports `synthetic-stub` for them — so the
downgrade is the `no_image_receiver` table and not the binary's age, while the
`NoClassDefFoundError`s *are* the binary's age. Two different causes for two
symptoms that look alike.

## Probes

`ADProbe.java` and `LLProbe.java`, written for this record. Both follow
`W7-1`'s rule — every row prints element **content**, and each catches
per-member so one refusal does not truncate the table. Both take a reflective
`LAYOUT` row under `--add-opens java.base/java.util=ALL-UNNAMED`, which is what
made the `elements.len` / `tail` comparison possible at all and is the single
step that separated "unwritten field" from "wrapped index".

---

## A34 2026-08-12 — the `--synthetic-jdk` arm, measured for the first time

Both families in this record were only ever measured on `--real-jdk` and
`--jdk-only`. The third mode had no binary. It has one now
(`C:/craton/synjdk-target/release/cratonvm.exe`, a `--features synthetic-jdk`
build, run as `--synthetic-jdk`), and the memory-note distinction applies
exactly as written: **the Cargo feature is the build, `--synthetic-jdk` is the
runtime mode**, and a shipping binary refuses the flag, so nothing before now
could have taken these rows.

Same `ADProbe2` source as the three-arm table above, same host, same day.

| row | HotSpot / `--real-jdk` / `--jdk-only` | `--synthetic-jdk` |
|---|---|---|
| `new ArrayDeque<>(List.of("a","b"))` | constructs | **`NoSuchMethodError: java.util.ArrayDeque.<init>(Ljava/util/Collection;)V`** |
| `ll.subList(0, 2)` | `[a, b]` | **`NoSuchMethodError: java.util.LinkedList.subList(II)Ljava/util/List;`** |
| `ll.equals(arrayList)` | `true` | **`false`** |
| `arrayList.equals(ll)` | `true` | `true` |
| `ll.listIterator()` forward | `abc` | `abc` |
| `ll.iterator()` | `abc` | `abc` |
| `ad.stream().count()` (`C5`, 16 elements) | `16` | `16` |

Three findings, in ascending order of how much they should worry a reader.

1. **`ArrayDeque.<init>(Collection)` is absent.** This is the exact constructor
   Part 1 identifies as the one that hits the full-buffer state every time
   (*"its real body is `this(c.size()); copyElements(c)`"*). In synthetic mode
   it does not exist at all, so Part 1's headline shape is unreachable there —
   the defect cannot occur because the constructor cannot be called. A missing
   method is a loud, diagnosable gap and is the least bad of the three.

2. **`LinkedList.subList(int,int)` is absent.** Same shape, same loudness.

3. **`LinkedList.equals` is IDENTITY comparison.** The row that surfaced it was
   the asymmetry — `ll.equals(al)` `false` against `al.equals(ll)` `true` — but
   the asymmetry is a symptom and the falsifier this section originally wrote
   was run rather than left standing. It fired the first way, and then a second
   probe discriminated the mechanism completely:

   | expression | HotSpot | `--synthetic-jdk` |
   |---|---|---|
   | `ll.equals(ll)` (same object) | `true` | `true` |
   | `ll.equals(ll2)` (equal `LinkedList`) | `true` | **`false`** |
   | `emptyLL1.equals(emptyLL2)` | `true` | **`false`** |
   | `ll.equals(arrayList)` | `true` | **`false`** |
   | `ll.equals(vector)` | `true` | **`false`** |
   | `ll.hashCode() == al.hashCode()` | `true` | **`false`** |
   | `ll.equals("a")` | `false` | `false` |
   | **`al.equals(al2)` (the CONTROL)** | `true` | `true` |

   Only the same-object row is true, including for two *empty* lists — that is
   `Object.equals`, not a broken element walk and not argument-type
   discrimination. `LinkedList` has no `equals` in the synthetic arm and falls
   through to identity; `ArrayList` in the same run is correct, which is the
   control that makes this a `LinkedList` finding rather than a mode-wide one.
   `hashCode` diverges with it, so a synthetic-mode `LinkedList` is also broken
   as a `HashMap` key. `indexOf(Object)` raises `NoSuchMethodError` beside them.

   This is not a refusal and not a gap — it is a **silent wrong answer**, and
   it is the same species this record already documents in strict mode
   (*"a refusal that reaches a caller with nowhere to put it stops being a
   refusal"*) arriving from the other side: nothing was refused, so nothing was
   recorded, and **the census cannot see it at all**.

**The mechanism, answered by the instrument rather than left as a falsifier.**
`--dump-native-registry` on the feature binary settles it: `java/util/LinkedList`
carries **36 registrations and `equals` is not one of them**. Nor is `hashCode`,
nor `indexOf`. Its sibling has all three:

| triple | `java/util/ArrayList` | `java/util/LinkedList` |
|---|---|---|
| `equals(Ljava/lang/Object;)Z` | `native-collections/src/lib.rs:4307` `[bridge]` | **absent** |
| `hashCode()I` | `native-collections/src/lib.rs:4306` `[bridge]` | **absent** |
| `indexOf(Ljava/lang/Object;)I` | `native-collections/src/lib.rs:4222` `[bridge]` | **absent** |
| `contains(Ljava/lang/Object;)Z` | present | present, `lib.rs:31733` `[bridge]` |

So this is **not** a last-write-wins loser and **not** a `NativeKind` drop — it
is an unimplemented family, and the two modes hide it for the same reason from
opposite directions: `--real-jdk` and `--jdk-only` both have real
`AbstractList.equals`/`hashCode` bytecode to inherit, and `--synthetic-jdk` has
none, so the call lands on `Object`. `contains` being present next to `equals`
being absent is the tell that this was an omission rather than a decision.

The generalisation, which is this campaign's twin-drift shape in a new place:
**`ArrayList` and `LinkedList` are twins and the `equals`/`hashCode`/`indexOf`
family was given to exactly one of them.** Diff the two receivers' registration
sets rather than checking that the one you are looking at "has natives".
