# C7-3 — `ll_get`'s overlay-first arm is stale by construction, and four public `LinkedList` methods were writing behind it

**Status:** FIXED this lane for the four reachable writers; the overlay design
itself stays OPEN. Lane C7, 2026-08-12. **No CratonVM binary was run** — the
"after" column is PREDICTED. The bytecode evidence is `javap -p -c` against
JDK 25.0.3+9 and is not predicted.

---

## 1. What "overlay-first" means, and why it is not merely *able* to answer stale

`ll_get` (`native-collections/src/lib.rs:31582` pre-edit):

```rust
fn ll_get(ctx: &dyn NativeContext, this: ObjectRef, name: &'static str) -> Value {
    {
        let ov = ll_overlay().lock()…;
        if let Some(v) = ov.get(&widened_obj_key(ctx, this)).and_then(|m| m.get(name)).copied() {
            return v;                       // <-- the overlay-first arm
        }
    }
    // Overlay MISS: fall back to the real JDK heap field.
    if let Some(real) = ll_real_field(name) { … ctx.get_field(this, slot) }
    Value::Object(None)
}
```

`ll_overlay()` is a process-global `Mutex<HashMap<usize, HashMap<&str, Value>>>`
keyed by `widened_obj_key` (the object's identity hash, generation-tagged). It
stores three names — `head`, `tail`, `size` — which `ll_real_field` maps to the
real declared fields `first`, `last`, `size`.

`ll_set` (`:31607`) writes the overlay **and mirrors to the real field**. So the
flow is asymmetric and the asymmetry is total:

```text
   native write  ->  overlay  AND  real field      (ll_set mirrors)
   real  write   ->  real field only               (nothing mirrors back)
   native read   ->  overlay if present, else real field
```

**The sharp point, which "can it answer stale?" understates:** the overlay is
seeded by `LinkedList.<init>` (registered, `native_ll_init` → `ll_set`). Every
natively constructed `LinkedList` therefore has all three overlay entries from
its first instruction. The overlay-MISS arm is not a fallback that fires
occasionally — for such a list it **never fires again for the rest of the
object's life**. Any real-bytecode write to `size`/`first`/`last` is invisible to
every native reader, permanently, and the two owners never reconcile.

(The MISS arm is not dead. It is the deserialization path: `readObject` does not
run `LinkedList.<init>()`, so a deserialized list has an empty overlay and reads
correctly from the heap. That is exactly what the arm's own comment says it is
for, and it is why the arm must not simply be deleted.)

`native_ll_iterator`'s comment (`:33141-33159`) records the reproducer once
(real `ListItr.remove()` → real `unlink` decrements the real `size` → `ll_get`
answers the old size → the next real iteration walks off the end →
`NullPointerException: Cannot read field "item"` at
`LinkedList$ListItr.previous`) and concludes: *"Handing iteration to real
bytecode would put a second writer on state the natives own."* That conclusion is
right. **It was only half-applied.**

## 2. The four second writers that were still reachable

Enumerated by diffing every method `javap -p java.util.LinkedList` declares
against every triple the registrars key on `java/util/LinkedList` (the class
registrar at pre-edit `:31666-31978`, plus the bulk-op registration of
`addAll(Ljava/util/Collection;)Z` at `:36719`).

| method | real bytecode | why it was a writer |
|---|---|---|
| `pollFirst()Ljava/lang/Object;` | `getfield first` … `invokevirtual unlinkFirst` | real `unlinkFirst` writes `first`, `size`, `modCount`. **Not** covered by the `java/util/Deque` bridge: `register_queue_deque_interface_natives` registers `peekFirst`/`peekLast` and no `pollFirst`/`pollLast` — and an interface row would not win anyway, because `LinkedList` declares the method (C7-2 §2) |
| `pollLast()Ljava/lang/Object;` | `getfield last` … `invokevirtual unlinkLast` | same shape |
| `addAll(ILjava/util/Collection;)Z` | links the new nodes inline | writes `first`/`last`/`size`. Its no-index sibling `addAll(C)Z` **was** registered — this is the half of the pair that was missed |
| `descendingIterator()Ljava/util/Iterator;` | `new LinkedList$ListItr; dup; …; invokevirtual size; invokespecial ListItr.<init>(LinkedList;I)` | it does **not** go through the registered `listIterator(int)`. It constructs the real node-live `ListItr` **directly**, so `remove()` calls real `LinkedList.unlink`. This is precisely the path `native_ll_iterator`'s comment names as its measured reproducer, and it was the one door left open |

Verbatim, so the fourth row is not a reading:

```text
class java.util.LinkedList$DescendingIterator implements java.util.Iterator<E> {
  private final java.util.LinkedList<E>.ListItr itr;
  private java.util.LinkedList$DescendingIterator(java.util.LinkedList);
    Code:
        …
        15: new           #19   // class java/util/LinkedList$ListItr
        18: dup
        19: aload_0
        20: getfield      #7    // Field this$0:Ljava/util/LinkedList;
        23: aload_0
        24: getfield      #7    // Field this$0:Ljava/util/LinkedList;
        27: invokevirtual #21   // Method java/util/LinkedList.size:()I
        30: invokespecial #27   // Method java/util/LinkedList$ListItr."<init>":(Ljava/util/LinkedList;I)V
        33: putfield      #30   // Field itr:Ljava/util/LinkedList$ListItr;
```

**Everything else was checked and is benign**, and the checking is the point —
this list is four because the other twelve unregistered methods were each read:

* `remove()`, `element()`, `offerFirst`, `offerLast`, `push`, `pop` are one-line
  `invokevirtual`s onto `removeFirst`/`getFirst`/`addFirst`/`addLast`, all of
  which are registered. They reach the natives.
* `peekFirst`, `peekLast`, `indexOf`, `lastIndexOf` only **read** — and they read
  correctly, because `ll_set`'s mirror keeps `first`/`last`/`size` and the node
  `item`/`next`/`prev` chain truthful. (`toArray(T[])`'s registrar note,
  corrected 2026-08-11, already measured this reflectively.)
* `clone()` writes only the fresh clone's own `first`/`last`/`size` to null/0
  before repopulating it through the native `add`; the clone's overlay is empty
  at that moment, so its MISS arm reads the values `clone()` just wrote.

## 3. The fix, and what it is not

Four registrations and four bodies in `native-collections/src/lib.rs`
(pre-edit anchor: immediately after `registry.register(c, "poll", …)` at
`:31742`):

```text
  pollFirst           -> native_ll_poll_first        (delegates to native_ll_poll)
  pollLast            -> native_ll_poll_last
  addAll(I,Coll)      -> native_ll_add_all_at
  descendingIterator  -> native_ll_descending_iterator
```

`native_ll_descending_iterator` builds `ll_snapshot_array`, reverses it in place,
and hands it to `real_snapshot_iterator` with
`SnapshotItrRoute::LinkedList` — the same construction
`native_ts_descending_iterator` (`:44468`) already ships. `hasNext`/`next` are
then real `java/util/Arrays$ArrayItr` bytecode over real fields; only `remove()`
routes back through the natives. On an image with no real `Arrays$ArrayItr`
(a `synthetic-jdk` build) it falls back to
`make_fabricated_iterator_from_array`, which is what every other snapshot
iterator in the crate answers there.

**This is not a fix to the overlay.** It restores the invariant the design
depends on — *the natives are the single writer* — by closing the four public
methods that violated it. The overlay-first read is still authoritative, the
heap→overlay direction is still unmirrored, and any *new* unregistered mutator
re-opens the hole. Two residuals stated so a later reader does not have to
rediscover them:

1. `SnapshotItrRoute::LinkedList`'s own doc records that its `remove()` deletes
   the **first occurrence** of the returned element rather than unlinking the
   exact node. On a list holding duplicates, `descendingIterator().remove()`
   therefore deletes the wrong one of an equal pair. The alternative it replaces
   deleted the right node and then corrupted `size` for the rest of the list's
   life, so this is the smaller residual, not the absence of one.
2. `LinkedList.reversed()` (JDK 21+, `ReverseOrderLinkedListView`) is
   unregistered. Its mutators delegate to the real public methods, which are now
   all registered, and its `iterator()` is `descendingIterator()`, which is now
   registered — so it is covered transitively rather than by inspection of the
   view class itself. Unmeasured.

## 4. The fixture, and why it is not vacuous

`regression-suite/src/RJdkMapViews.java`, sections `linkedListPollEnds`,
`linkedListAddAllAt`, `linkedListDescendingIterator`. **74 checks, green on
HotSpot 25.0.3+9** (transcript below). It has not been run on CratonVM.

`P2` §8.2's `ArrayDeque` control came back verdict-neutral because the `RJdk*`
corpus only touches a deque's ENDS and so never called the `delete(i)` the record
named. The discipline that follows is: *state which method the workload actually
calls.* For each block:

* `linkedListPollEnds` calls **`pollFirst()` and `pollLast()`** — the two methods
  in question — and then reads back through `size()`, `toString()`, `getFirst()`,
  `getLast()`, `isEmpty()`, all of which are natives reading `ll_get`. It also
  drains the list to empty through those two doors and then reuses it, because a
  `size` that drifted by one is invisible until the chain is walked past its end.
* `linkedListAddAllAt` calls **`addAll(int, Collection)`** at the middle, the
  head, and at `index == size`, then reads back through `size()`, `toString()`,
  `get(int)` and `getLast()`; it also asserts the out-of-range case throws
  *and leaves the list unchanged*, which is the property a "check bounds after
  the first link" implementation loses.
* `linkedListDescendingIterator` calls **`descendingIterator()`** for a full walk
  (`"rqp"`), then again for a `next()` + **`remove()`**, then reads back through
  `size()`, `toString()`, `contains()` and `getLast()`, and finally iterates the
  list forwards — the operation that used to `NullPointerException` on
  `Node.item` once `size` had drifted.

PREDICTED on the **pre-fix** binary: `pollFirst()` returns `"a"` (real bytecode
does unlink correctly), then `l.size()` answers **4** where HotSpot says 3, and
`l.toString()` walks from the stale overlay `head` — so the first three
assertions of `linkedListPollEnds` fail. That is what makes the fixture a
discriminator rather than a description.

PREDICTED on the **post-fix** binary: green, 74/74, byte-identical to the
HotSpot transcript the harness diffs against.

```text
$ java -cp out RJdkMapViews
CK RJdkMapViews valuesIdentity ok
CK RJdkMapViews valuesLiveness ok
CK RJdkMapViews pollEnds ok
CK RJdkMapViews addAllAt ok
CK RJdkMapViews descendingIterator ok
CK RJdkMapViews checks=74
PASS RJdkMapViews (74 checks)
rc=0
```

One assertion is worth flagging as the fixture's own soft spot:
`l.indexOf("d") == 3` reads through **unregistered real bytecode** walking the
mirrored node chain. It is deliberate — it cross-checks that `ll_link_before`
leaves the real chain truthful — but if it is the only red line, the finding is
about the mirror, not about `addAll`.

## 5. Nominations

**N1 (`regression-suite/run.sh`, not mine).** Register the new fixture in the
JDK-only corpus.

- exact old text: `RJdkForeign RJdkEnumerations RJdkAsyncChannel"`
- exact new text: `RJdkForeign RJdkEnumerations RJdkAsyncChannel RJdkMapViews"`

(That is the tail of the `JDKONLY_CLASSES=` assignment at `regression-suite/run.sh:119`.)

**N2 (`native-collections/src/lib.rs`, mine, NOT taken).** `ll_get`'s
overlay-first arm should be retired outright in favour of the real fields, with
the overlay kept only for names the JDK has no field for. That is the
reclassification `W7-16` concluded and it is a much larger change than this one:
it needs `head`/`tail` to stop being a separate truth, and it needs the
deserialization path (§1) to keep working. This lane deliberately did not attempt
it, because the invariant repair in §3 is a strict prerequisite either way — a
lane that deletes the overlay-first arm while four public methods are still
unregistered writers gets a *different* pair of disagreeing owners, not one.

**N3 (measurement, nobody's source).** The census delta. Four new `Bridge`
registrations that shadow real bytecode:

```text
  bridge.rows                       +4
  bridge.shadows_bytecode           +4
  bridge.shadows_bytecode_anywhere  +4
  bridge.without_acc_native         +4
  registrations.bridge              +4
  total_rows                        +4
  registrations.synthetic-stub      unchanged (all four are Bridge)
  stub ratchet                      unchanged
  kind map                          +4 rows, all `bridge` / kind_stated 0
```

These fold into the **same** single Linux re-freeze
`P2-COLLECTIONS-SHADOWS-20260812.md` §5.1 and §8.5 already owe. They cannot be
taken on a Windows host — both gate scripts exit 2 ("REFUSING"). **A fifth row is
a finding.**
