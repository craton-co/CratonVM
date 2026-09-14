# W7-20 — a refusal laundered into a wrong answer, and the paired mint that stops producing them

> **RUN AND VERIFIED 2026-08-12 (lane A32, record triage). `al.equals(ll)` IS
> `true` UNDER `--jdk-only`. THE PREDICTED CENSUS MOVEMENT IS CONFIRMED
> EXACTLY, INCLUDING THE PART THAT WAS PREDICTED *NOT* TO MOVE.**
>
> This record's status line says NOT REBUILT and its Part 1 table was taken on a
> binary that predates W2-1. Measured here on `cratonvm-merged-dev.exe` against
> `jdk-25.0.3.9-hotspot`, HotSpot 25 as the same-session oracle.
>
> **Part 1's table, re-taken. `--jdk-only` is now identical to HotSpot on every
> row but the one this record exempts:**
>
> | row | HotSpot 25 | `--jdk-only` |
> |---|---|---|
> | `al.equals(ll)` **(THE ROW)** | `true` | **`true`** |
> | `ll.equals(al)` | `true` | `true` |
> | `al.hashCode()==ll.hashCode()` | `true` | `true` |
> | `al.containsAll(ll)` | `true` | `true` |
> | `ll.listIterator().getClass()` | `java.util.LinkedList$ListItr` | `cratonvm.internal.LinkedListSnapshotListItr` |
> | `al.toString()` / `ll.toString()` | `[a, b, c]` | `[a, b, c]` |
> | `new ArrayList<>(ll)` / `new HashMap<>(tm)` | `[a, b, c]` / `{k=v}` | same |
>
> The `NoClassDefFoundError`s that filled the strict column are gone, and the
> one remaining difference is the carrier's name — exactly the exemption
> *Verification, once this is built* names ("identical … on every row except
> `ll.listIterator().getClass()`").
>
> **Part 3(b) is closed by run.** `listIterator() instanceof ListIterator` is
> `true`, and an erased-type `(ListIterator) x` cast through `Object` succeeds
> and iterates. The `jdk_interfaces` arm landed.
>
> **The error channels behave in the direction intended, checked with a
> throwing element rather than only with a refusal** — an element whose own
> `toString`/`hashCode` raise:
>
> ```text
> HotSpot 25 / --jdk-only  (identical)
> list.toString()  IllegalStateException: boom-toString     <- propagates
> list.hashCode()  IllegalStateException: boom-hashCode     <- propagates
> ```
>
> Neither truncates to `[]`, neither fabricates a `ClassName@hash`, and neither
> substitutes an identity hash. That is the `Compatible`-mode half of the change
> — the one this record says is its only regression risk — behaving as HotSpot
> does.
>
> **The census moved exactly as predicted, and the negative prediction is the
> load-bearing one.** From `--jdk-only --explain-jdk-only --jdk-only-report`:
>
> * the `compatibility-class-requested` row for
>   `cratonvm/internal/LinkedListSnapshotListItr` is **gone** — the string does
>   not occur anywhere in the census;
> * the nine `synthetic-native-registered` rows have **left** that population;
> * **the four `native-shadows-bytecode` rows stay**, and they are precisely the
>   four named: `java/util/LinkedList` `<init>(Ljava/util/Collection;)V`,
>   `iterator()`, `listIterator()`, `toString()`.
>
> Do not read the green rows as "the gap is closed" — this record already says
> so, and the census now proves it from the tree rather than predicting it.
> `java/util/LinkedList$Itr` is **still** requested and refused under
> `--jdk-only` (it is one of the 14 surviving `compatibility-class-requested`
> rows), yet `hashCode` and `containsAll` answer correctly anyway. That is the
> deferred-refusal shape this record files under *Not fixed, with the verdict*
> for `alloc_real_snapshot_iterator_of` — the refusal is routed, not lost —
> and it is W2-1's family, not this one's.
>
> **Source audit, every claim checked individually rather than sampled:**
>
> * **All fourteen helpers carry an error channel.** Each signature was read.
>   `collection_elements_generic` (`native-collections/src/lib.rs:3950`),
>   `collect_via_real_iterator` (`:5246`), `collect_via_real_iterator_once`
>   (`:5281`), `al_or_collection_elements` (`:5378`),
>   `collect_collection_elements_or_real` (`:36773`), `collect_entries_any`
>   (`:11616`), `collect_entries_via_iterator` (`:11725`) and `_inner`
>   (`:11747`), `prim_stream_values` (`:19595`), `stream_source_elems`
>   (`:18136`), `group_key_equal` (`:3589`), `list_element_matches` (`:3641`),
>   `element_hash_code` (`:7405`), `obj_to_display_string` (`:3326`) — every one
>   returns `Result<_, MethodCallFailed>`. The two companions are converted too:
>   `pinned_array_search` (`:3679`) and `ll_pinned_find` (`:32798`).
> * **Both gates are open.** `cratonvm/internal/LinkedListSnapshotListItr` is in
>   `VM_SERVICE_RECEIVERS` at `native-api/src/no_image_receiver.rs:267`, in
>   correct sort position (between `cratonvm/Wp71JdbcSpi` and
>   `cratonvm/internal/SystemLogger`), and is **absent** from
>   `VM_MINTED_STAND_IN_RECEIVERS` — line 198 there is a tombstone comment. The
>   `binary_search` this record warns can fail silently will find it.
> * All nine natives use `register_with_kind(…, NativeKind::Bridge)`
>   (`native-collections/src/lib.rs:31913-31972`), and the mint calls
>   `ensure_vm_internal_class` **inside** the `rooted_across` closure
>   (`:32036`, `:32062`) — the GC-safety correction this record makes to W7-16's
>   recorded hunk was applied as written.
> * `jdk_interfaces` arm present at `classloading/src/class_manager.rs:11198`.
>
> **Three corrections to this record's own text:**
>
> 1. **The prose says "Nine helpers"; its own table lists fourteen**, and
>    fourteen (plus two companions) are what the tree has. The table is right.
> 2. **§(a) cites `jdk-only-kind-map-25-linux.tsv` lines 283–291; the nine rows
>    are at 379–387**, and they now read **`bridge`**, not `synthetic-stub`. The
>    re-freeze §(a) demands has happened. Its header discloses the rows were
>    hand-edited and records why `kind_stated` stayed `1` — because the nine
>    registrations moved to `register_with_kind` in the same change.
> 3. **`probes/LaunderProbe.java` is still NOT in the tree.** Confirmed against
>    `git ls-files`, a repo-wide glob and a full-text grep: the only occurrences
>    of the name are in this record and W7-62. The *Probes* section at the end
>    still says it was "written for this record" without the caveat the inline
>    note carries. **This record's Part 1 evidence is therefore unscheduled and
>    unreproducible from the tree** — the table above was re-taken from a
>    reconstruction, held in a scratch directory outside the worktree so it does
>    not trip the source-witness gates. `probes/` is not run by
>    `regression-suite/run.sh` at any `SUITE=` value in any case;
>    `probes/ListItrInterfaceProbe.java`, which W7-62 added, does exist.
>
> **Disposition: FIXED, verified by execution.** The open items are the ones
> this record already scopes out — the `listIterator` shadow retirement (a
> collections reclassification), and the Linux-only ratchet re-freeze in §(a),
> which cannot be settled on this Windows host: both artefacts are keyed
> `25/linux`, so both look up `25/windows`, find nothing and exit 2.

**Status: DIAGNOSED and FIXED IN SOURCE 2026-08-11, NOT REBUILT. Both
out-of-file items taken 2026-08-12 — see the block above "Out-of-file patch"
and W7-62-ratchets-and-dead-code.md.** Nine
helpers in `native-collections/src/lib.rs` gained an error channel, and
`cratonvm/internal/LinkedListSnapshotListItr` now mints through the
VM-internal door with its natives kept `Bridge` — both halves in one commit,
as `W7-16-arraydeque-and-linkedlist-residuals.md` requires.

Every "before" row below is an observation taken by running the already-built
binary at `C:/craton/CratonVM/target/release/cratonvm.exe` against Temurin
`jdk-25.0.3.9-hotspot` on windows/x64, one binary per row with only the mode
flag differing. **Nothing here claims a source change works.** No `cargo`
command was run on this branch.

Branch: `fix/strict-refusal-laundered-into-wrong-answer-20260811`.
Files changed: `native-collections/src/lib.rs`,
`native-api/src/no_image_receiver.rs`, and this record.

Predecessors: `W7-16-arraydeque-and-linkedlist-residuals.md` (which measured
defect 1 and wrote both halves of defect 2 out verbatim),
`W7-13-strict-mh-insert-wrapper.md` (the `VmInternal` door),
`W7-1-treemap-views-and-iterator-remove-contract.md` (why an empty answer is
the dangerous one), `W2-1-strict-refuses-the-synthetic-stream-stack.md`.

---

## Part 1 — the species

Strict mode's whole value is that it **refuses** rather than fabricates. A
helper that catches the refusal and returns an empty `Vec` converts the one
honest failure mode back into the dishonest one, and leaves the census
believing the refusal was observed: the violation *is* recorded, so the run
looks measured, while the program is handed a wrong answer.

An empty collection is also the failure mode that reads as a **pass** anywhere
a caller only iterates. That is not a hypothetical: four `TreeMap` navigable
views answered `{}` for months for exactly this reason
(`W7-1-treemap-views-and-iterator-remove-contract.md`).

### The worked instance, re-measured here

`LaunderProbe.java`, following `W7-1`'s rule — every row prints content, and
each row catches per-member so one refusal does not truncate the table.
`al = new ArrayList<>(List.of("a","b","c"))`, `ll = new LinkedList<>(same)`.

| row | HotSpot 25 | `--real-jdk` | `--jdk-only` |
|---|---|---|---|
| `al.toString()` | `[a, b, c]` | `[a, b, c]` | `[a, b, c]` |
| `ll.toString()` | `[a, b, c]` | `[a, b, c]` | `[a, b, c]` |
| `ll.equals(al)` | `true` | `true` | *NoClassDefFoundError: `cratonvm/internal/LinkedListSnapshotListItr`* |
| **`al.equals(ll)`** | `true` | `true` | **`false` — no exception** |
| `al.hashCode()==ll.hashCode()` | `true` | `true` | *NoClassDefFoundError: `java/util/LinkedList$Itr`* |
| `ll.listIterator().getClass()` | `java.util.LinkedList$ListItr` | `cratonvm.internal.LinkedListSnapshotListItr` | *NoClassDefFoundError* |
| `al.containsAll(ll)` | `true` | `true` | *NoClassDefFoundError: `java/util/LinkedList$Itr`* |
| `new ArrayList<>(ll)` | `[a, b, c]` | `[a, b, c]` | `[a, b, c]` |
| `new HashMap<>(treeMap)` | `{k=v}` | `{k=v}` | `{k=v}` |

The `al.equals(ll)` row is the whole point, and the rows around it are what
make it legible: **the same underlying refusal is loud on four rows and silent
on one.** The difference is not the refusal, it is the return type of the
helper the refusal lands in. `native_al_equals`'s cross-layout arm went
through `collection_elements_generic`, which was `-> Vec<Value>` and mapped
the `NoClassDefFoundError` from its own `iterator()` call to `Vec::new()`, so
three elements were compared against zero. The other four rows go through
`Result`-returning paths and were already honest.

The `java/util/LinkedList$Itr` refusals are **not findings of this record.**
They are `W2-1`'s already-fixed-in-source iterator gap showing through a
pre-built binary that predates the fix — see *Binary provenance*.

### The discrimination line

One rule, applied to all nine helpers, and it is what keeps the change from
being "make everything throw":

* **`Err(..)` propagates.** The call was refused, or it threw. Under
  `--jdk-only` that is a policy `NoClassDefFoundError`; in `Compatible` it is
  an exception from the receiver's own bytecode, which HotSpot also propagates
  out of `AbstractList.equals` / `Collection.toString` / `List.contains`
  rather than truncating.
* **`Ok(..)` with nothing usable stays as it was.** A null iterator, a void
  return, a primitive where an object was expected, a callee that returns a
  non-`Int` from `equals`/`hashCode`. The call did not fail; it gave us
  nothing to work with. That is the documented best effort for a foreign
  collection whose layout we cannot model, and every helper keeps it.

The second half is load-bearing. These helpers are registered on
`Collection`/`List`/`Map`/`Iterable` **interfaces**, so they intercept every
third-party and user collection in the process; converting "I could not read
this" into an exception would be a far larger behaviour change than the one
being fixed, and would not be a fix at all.

---

## Part 2 — the laundering sweep

Method: index every top-level `fn` in `native-collections/src/lib.rs` (1,456 of
them), intersect with every line that calls `invoke_virtual` / `invoke` /
`invoke_static` / `ensure_class_initialized` / `try_ensure_synthetic_class` /
`try_alloc_synthetic`, and keep the ones whose return type has no error
channel. Then, for each call site of each survivor, walk the brace depth
backwards to find the enclosing function **and** any enclosing closure, so that
"the caller can carry an error" is checked against the construct the `?` would
actually return from, not against the function it happens to sit in.

### Fixed

| helper | what it swallowed | caller can carry? | action |
|---|---|---|---|
| `collection_elements_generic` | `iterator()` / `hasNext()` / `next()` on the receiver | yes — 2 sites, both `native_al_equals` | `-> Result`; failure held in a local so the per-element pin unwind stays one LIFO path |
| `collect_via_real_iterator` | same three calls, no pins | yes — 3 sites in `al_or_collection_elements` and `collect_collection_elements_or_real` | `-> Result` |
| `collect_via_real_iterator_once` | its wrapper | yes | `-> Result`; the re-entrancy flag is now cleared on the failing path too, or one refusal would latch it and every later fallback walk on that thread would answer empty |
| `al_or_collection_elements` | `size()` read as `0` ⇒ no fallback walk ⇒ the empty heuristic snapshot returned | already `Result` | propagate; body split around its pin so the four `?` release `this_pin` instead of leaking it |
| `collect_collection_elements_or_real` | `size()` as `0`, `toArray()` as "no array" | already `Result` | propagate; this is the helper behind the copy constructors, `addAll`, `removeAll`, `retainAll`, `containsAll` |
| `collect_entries_any` | `Map.isEmpty()` read as "empty" ⇒ the `entrySet()` walk skipped entirely | yes — **15** sites, all `MethodCallResult` or `Result` | `-> Result`, `?` at every site |
| `collect_entries_via_iterator` / `_inner` | `entrySet()`, `iterator()`, `hasNext()`, `next()`, and — worse — `getKey()`/`getValue()` via `.ok().flatten().unwrap_or(null)`, which put a **null key** into the destination as a real entry | yes | `-> Result`; failure recorded so the single `unpin_native_roots(source_pin)` truncate still covers `set_pin`, `it_pin` and every per-entry pin, and `IterCollectGuard::drop` covers the two early returns |
| `prim_stream_values` | `let _ = materialize_lazy_stream(..)`, plus a catch-all over a **real** JDK pipeline's `toArray` ⇒ "that sub-stream was empty", concatenated into `flatMap`'s result | yes — 6 sites in the three `*Stream.flatMap` natives | `-> Result`; each site unwinds `f_pin` exactly as the existing `apply` arm does |
| `stream_source_elems` | `let _ =` on the lazy-spliterator drain ⇒ slot 0 read as the source with whatever it held before | yes — 2 sites, both `Result<PullStep, _>` | `-> Result`, unpinning before it propagates |
| `group_key_equal` | `if let Ok(..)` on the key `equals` ⇒ a failing comparison **opened a new group**; `groupingBy` answered one bucket per element | yes — 3 sites in `native_stream_collect` | `-> Result` |
| `list_element_matches` | `if let Ok(..)` on the element `equals` ⇒ the **opposite** answer, acted on: `remove` reports "not present" and mutates nothing, `retainAll` drops the element | yes — 22 sites across ArrayList, LinkedList, ArrayDeque, LinkedHashMap, TreeMap, HashSet, COWAL, LBQ, PriorityQueue, Stack and the CHM key-set view | `-> Result`; `pinned_array_search` and `ll_pinned_find` also `-> Result`, recording the failure so their `unpin_native_roots` still runs, and their 10 callers take `?` |
| `element_hash_code` | `hashCode()` failure ⇒ the **identity** hash, a plausible number ⇒ the aggregate `List`/`Set`/`Map` hash comes out stable and wrong, so the collection is filed under a bucket its own equal twin will never be found in | yes — 6 production sites | `-> Result`; each unwinds its own accumulation pin. The two in-file unit tests take `.unwrap()` |
| `obj_to_display_string` | `toString()` failure ⇒ `ClassName@hash`, so a refusal inside an element's own `toString` came out of `list.toString()` looking like ordinary output | yes — 22 sites, every `toString` native plus `Collectors.joining` and `Stream.sorted`'s string-comparator fallback | `-> Result`; two iterator chains take `collect::<Result<..>>()?` rather than a `?` inside the closure, which in `Stream.sorted` also stops the stream being ordered by a fabricated `ClassName@hash` |

`group_key_equal`, `list_element_matches` and `element_hash_code` are worth
naming together: `map_keys_equal` and `map_hash_key`, in the same file, already
carry doc blocks explaining that this exact swallow is a bug and were fixed for
it. The other three copies of the same shape were left. **When a defect is
fixed in one helper, grep the file for the idiom, not for the call site** —
the second instance of that lesson this session.

### Not fixed, with the verdict

| helper | swallows | verdict |
|---|---|---|
| `alloc_real_snapshot_iterator_of` / `alloc_real_array_iterator` | `ensure_class_initialized(..).ok()?` | **Not a launderer.** `None` routes to `make_fabricated_iterator_from_array`, which goes through `try_alloc_synthetic` and therefore raises the refusal properly at that site. The refusal is deferred, not lost. Checked, not assumed. |
| `alloc_backing_map`, `alloc_hs_backing`, `alloc_linked_hash_map` | `ensure_class_initialized("java/util/HashMap" / "…LinkedHashMap")` failure ⇒ `ClassId::new(0)` | **Out of species.** §5 refuses *compatibility stand-ins*; a real `java.util` class is never refused in any mode. The only reachable failure is a broken image or OOM, which is a different (smaller) defect. |
| `collections_empty_singleton`, `cf_nil` | lookups on real `java/util/Collections` / `CompletableFuture` | Same reason. |
| `box_primitive_stream_elements`, `tree_key_to_value`, `ksv_boxed_true` | `Integer.valueOf` / `Boolean.valueOf` failure ⇒ a null or a raw primitive | Same reason — the receivers are `java.lang` wrappers. `tree_key_to_value`'s `unwrap_or(Value::Object(None))` does fabricate a null key, so it is the same *shape*; it is not the same *channel*, and this branch cannot rebuild to justify widening the sweep past the stated scope. |
| `pbq_seed_real_lock` | `ReentrantLock` construction ⇒ leave the field unset | Out of species, and the signature is `-> ()`. |
| `interrupt_tpe_workers` | `AtomicInteger.get/set` on a real `ThreadPoolExecutor` | **Out of species by role.** Its `false` selects the synthetic fallback path; it is not an answer handed to the program. |
| `al_state` | returns `(None, 0)` for a wrong-layout receiver | **Deliberate guard, not a swallow.** The sentinel makes the caller fall back to virtual dispatch, and the doc block records the SIGSEGV it exists to prevent. No `invoke` is involved. |
| `map_keys_equal_identity`, `class_name_is`, `read_value_slice`, `is_synthetic_backed_collection` | — | Scanner false positives; no fallible call in the body. |

### Where a caller genuinely cannot carry a failure

Two, and both are real constraints rather than places to launder:

1. **`pbq_seed_real_lock(ctx, this) -> ()`.** A seeding routine called for
   effect. Giving it an error channel means giving one to
   `native_pbq_init`'s seeding step, which is a separate change with a
   separate blast radius.
2. **`element_hash_code`'s two in-file unit-test call sites**
   (`unbox_wrapper_requires_jdk_wrapper_class`). `assert_eq!` cannot carry a
   `Result`; they take `.unwrap()`, which is correct — the mock context cannot
   produce an `Err`, so an `unwrap` there is an assertion, not a swallow.

Everything else in the "fixed" table had a caller that could carry, which is
why it was fixed. That is the finding: **the error channel was almost never
missing because it could not exist.** It was missing because a `Vec` is easier
to return than a `Result`, and the cost only became visible when strict mode
started refusing things.

---

## Part 3 — the paired mint

`cratonvm/internal/LinkedListSnapshotListItr` was refused by **two independent
gates**, and `W7-16` measured that clearing either alone makes things worse:
clearing only the class moves the failure from `NoClassDefFoundError` at the
mint to `UnsatisfiedLinkError` at the first `hasNext()`. Both halves land here,
in one commit.

### Gate 2 first, because it is the one that is invisible

Re-measured on the pre-built binary, independently of `W7-16`, via
`--dump-native-registry`:

```text
9 registrations for cratonvm/internal/LinkedListSnapshotListItr
all nine: kind = synthetic-stub
```

`register_linked_list_natives` sets `NativeKind::Bridge` for its whole body and
does not restore the previous category until after these nine. `Bridge` is what
the registration site asks for; `synthetic-stub` is what
`receiver_declared_by_no_supported_image` overrides it to, because the name was
on `VM_MINTED_STAND_IN_RECEIVERS`. The `JdkOnly` arm of `register()` then
returns without inserting them.

Moved to `VM_SERVICE_RECEIVERS` — contract §11's "a reviewed VM service", and
`Bridge` is the only tag that survives `NativeKind::allowed_in(JdkOnly)`.

**Why it belongs there and not on the stand-in list.** It stands in for
nobody. It was deliberately *not* named `java/util/LinkedList$ListItr`: that
name resolves to the real 5-field class, whose layout mangled the `Int` cursor
write into the real `next:Node` slot and made `next()` never advance, so
`AbstractList.equals` compared element 0 forever. The `cratonvm/` name is what
keeps the layout ours. It landed on the stand-in list **by prefix, not by that
test**, and the module doc now says so — a `cratonvm/…` name is not
self-evidently a stand-in.

### Gate 1

`try_alloc_synthetic` stamped `ClassOrigin::CompatibilityStub`, which
`--jdk-only` forbids. Now minted through `ensure_vm_internal_class`
(`ClassOrigin::VmInternal`), which contract §1 item 6 permits in every mode —
the door `W7-13-strict-mh-insert-wrapper.md` established for the ten
`MethodHandles` combinator carriers, on the same test: *does the JVM
specification say a class file must exist for this name?* No image declares a
`cratonvm/…` name, nothing is being stood in for, and the three slots are the
`Object[]` snapshot, the `Int` cursor and the backing list.

### Verifying the recorded patches, and the two places they were wrong

`W7-16` recorded both hunks verbatim. Both were checked against the source
before applying, and both needed a correction:

1. **The mint hunk hoisted `ctx.alloc_object(it, 3)` OUT of `rooted_across`.**
   `alloc_object` can collect, and `arr` and `this` are stored into the result
   on the three lines immediately after, so that would have left both unrooted
   across a moving GC — a fresh instance of the Family-1 stale-`ObjectRef`
   shape this file has paid for repeatedly. The allocation stays **inside** the
   closure, exactly where `try_alloc_synthetic` performed it:

   ```rust
   let it = rooted_across(ctx, &mut [&mut this, &mut arr], |ctx| {
       let cid = ctx.ensure_vm_internal_class("cratonvm/internal/LinkedListSnapshotListItr", 3);
       ctx.alloc_object(cid, 3)
   });
   ```

   The `?` does go away, as recorded — `ensure_vm_internal_class` is
   infallible, which is the point of the door.

2. **The table entry's sort position was asserted, not assumed.**
   `table_is_sorted_and_unique` binary-searches `VM_SERVICE_RECEIVERS`, so an
   entry in the wrong place makes the predicate answer `false` for a name that
   *is* in the list, silently. `'L' < 'S'`, so
   `cratonvm/internal/LinkedListSnapshotListItr` goes before
   `cratonvm/internal/SystemLogger`. The record placed it there; confirmed.

Nothing else in either hunk needed changing. Both were already applied to
*neither* file, checked by reading, not by trusting the record's status line —
this campaign has fifteen records claiming a patch was never applied when it
was already in the tree, and the inverse costs just as much.

### What the census does and does not lose

Measured on the pre-built binary with `--jdk-only-report`, so this is the
*before* state:

```text
compatibility-class-requested  cratonvm/internal/LinkedListSnapshotListItr
compatibility-class-requested  java/util/LinkedList$Itr
native-shadows-bytecode        java/util/LinkedList.<init>(Ljava/util/Collection;)V
native-shadows-bytecode        java/util/LinkedList.iterator()
native-shadows-bytecode        java/util/LinkedList.listIterator()
native-shadows-bytecode        java/util/LinkedList.toString()
synthetic-native-registered    cratonvm/internal/LinkedListSnapshotListItr.{9 methods}
```

Predicted after (not measured — nothing was rebuilt): the
`compatibility-class-requested` row for the carrier disappears, and the nine
`synthetic-native-registered` rows leave that population because the kind is no
longer `synthetic-stub`. **The four `native-shadows-bytecode` rows stay**, and
`java/util/LinkedList.listIterator` is the one that actually names the defect:
CratonVM serves it from a native instead of running the JDK's bytecode. It is
keyed on the **real** class and on the registration, so no change to the
carrier can move it.

Do not read this as "the gap is closed". Retiring it means making the natives
stop owning `LinkedList` state and then dropping the `listIterator`
interception — a collections reclassification, not an iterator change. Until
then the carrier is how the one owner of the state is held, and that
constraint was re-measured in **`Compatible`** mode on the unmodified binary:
driving a real `ListItr.remove()` at a native `LinkedList` leaves
`list=[a, b]` with `size=3` where HotSpot gives `2`.

It is also a `java/util/*` name, which is `W2-1`'s lesson restated: **an
inventory scoped to `cratonvm/*` under-reports these gaps.**

---

## Which mode each change affects

| change | `--jdk-only` | `Compatible` |
|---|---|---|
| the nine error channels | a refusal now reaches the program as the `NoClassDefFoundError` the contract names, instead of as an empty collection / `false` / an identity hash / a `ClassName@hash` string | **only on the exceptional path.** No policy refusal exists in this mode, so the sole behaviour change is that an exception thrown by a receiver's own `iterator`/`next`/`equals`/`hashCode`/`toString`/`size`/`getKey`/`getValue` propagates instead of silently truncating or fabricating. That is what HotSpot does. Byte-for-byte unchanged on every non-throwing path. |
| the mint + retag | `listIterator()`, `listIterator(int)`, `subList`, `sort`, and `AbstractList.equals`/`hashCode`/`indexOf` against a foreign list stop refusing | unchanged — this mode fabricated through either door already. One second-order improvement: `fabricate_class` stops running a full-classpath rescan per carrier looking for bytes that cannot exist. |

---

## Out-of-file patch — (a) PARTLY APPLIED, (b) APPLIED, 2026-08-12

> **W7-62-ratchets-and-dead-code.md took both.** In summary, so nobody
> re-derives it:
>
> * **(a) the kind map is re-frozen; the JSON is not.** Twelve rows of
>   `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, by hand and disclosed
>   in its header on W7-56's convention — the nine below plus
>   `java/util/logging/Formatter.formatMessage` and
>   `LogManager.{getLogManager, getLogger}`, which this record did not know
>   about. **The nine were not frozen at what the tree produces:** the retag
>   also dropped their `kind_stated` from 1 to 0, which the gate refuses
>   one-way and correctly, so the nine registrations moved to
>   `register_with_kind(..., NativeKind::Bridge)` and the baseline holds
>   `bridge 1 1`. `jdk-only-bridge-ratchet.json` is deliberately left firing
>   with its derived movement written into its `note`: this section is right
>   that the values must come from a census, and a count hand-written too high
>   widens a slack-free ratchet.
> * **(a) also found a THIRD stale ratchet this record does not mention** —
>   `native-builtins/tests/stub_ratchet.rs`, stale in the firing direction by
>   at least +6, from the same 2026-08-11-evening retag wave.
> * **(b) the `jdk_interfaces` arm is applied**, with
>   `probes/ListItrInterfaceProbe.java` and a HotSpot 25 control transcript
>   beside it. It closes the `ClassCastException` rather than moving it; the
>   reasoning is in W7-62 §3.2.
> * **`probes/LaunderProbe.java`, which Part 1's table was measured with and
>   this record says was "written for this record", is NOT IN THE TREE.** Not
>   in `probes/`, not in `regression-suite/src/`. The measurement cannot be
>   re-run.
>
> **2026-08-12, second pass: the red now has THREE causes and only one of them
> is this record's.** Do not settle it with a single re-freeze — a conflated
> number is what made this ratchet unreadable in the first place. See
> *The census that settles this, and where it must be taken* below.

### (a) Two baselines under `scripts/baselines/` must be re-frozen

The retag moves nine registrations from `synthetic-stub` to `bridge`, and the
kind-map gate exists precisely to catch that. **This is the gate working, not
a problem with the gate** — do not re-freeze without reading the diff, which
is what its own header says.

`scripts/baselines/jdk-only-kind-map-25-linux.tsv`, lines 283–291, currently:

```text
cratonvm/internal/LinkedListSnapshotListItr	add	(Ljava/lang/Object;)V	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	hasNext	()Z	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	hasPrevious	()Z	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	next	()Ljava/lang/Object;	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	nextIndex	()I	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	previous	()Ljava/lang/Object;	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	previousIndex	()I	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	remove	()V	0	synthetic-stub	1	1
cratonvm/internal/LinkedListSnapshotListItr	set	(Ljava/lang/Object;)V	0	synthetic-stub	1	1
```

Each row's `kind` column becomes `bridge`. **Do not hand-edit it** — the file
is a frozen census, and the trailing columns are measured. Re-take it from a
real linux/25 census on a binary built from this branch and diff the result;
the nine rows above must be the *only* difference.

`scripts/baselines/jdk-only-bridge-ratchet.json` moves in the same direction
and is slack-free, so it fails until re-frozen. The counters that shift, and
the direction, all by nine: `registrations.bridge` **up**,
`registrations.synthetic-stub` **down**, `bridge.rows` **up**,
`bridge.without_acc_native` **up**, `bridge.class_absent` **up** (no image
declares the carrier). Exact values are deliberately not written here — they
must come from the census, not from this record's arithmetic. The `note` field
should say that the movement is the `LinkedListSnapshotListItr` retag and cite
this record.

### The census that settles this, and where it must be taken — 2026-08-12

**One command, and it must run on LINUX against JDK 25.**

```sh
JAVA_HOME=<jdk25> bash regression-suite/bridge-ratchet.sh
```

It boots `--real-jdk` against a real image, takes one schema-4 census, and
scores BOTH gates from it — `scripts/jdk-only-bridge-ratchet.py` and
`scripts/jdk-only-kind-map.py`. Two boots would be two objects; that is why the
script refuses to take a second census.

**The platform half is not a detail.** Both artefacts are keyed
`<jdk-feature>/<os>`: the JSON's only entry is `"25/linux"` and the kind map is
`jdk-only-kind-map-25-linux.tsv`. `jdk-only-bridge-ratchet.py`'s `host_os()`
derives the key from the running interpreter, and `bridge-ratchet.sh` derives
`--os` from `uname`. **On the Windows session host both gates therefore look up
`25/windows`, find nothing, and exit 2 — "REFUSING: no committed baseline"**,
which is not a pass, not a fail, and cannot re-freeze anything. Run it on the
Linux build host. Anyone who runs it here and reports "the gate did not fire"
has measured the absence of a baseline.

**Three contributions to the current red. Attribute them apart before
re-freezing; a single conflated number is what made this artefact unreadable.**

| # | cause | effect on `jdk-only-bridge-ratchet.json` | effect on `stub_ratchet.rs` |
|---|---|---|---|
| (a) | the retag this record is about, plus `Formatter.formatMessage` and `LogManager.{getLogManager,getLogger}` | `bridge.without_acc_native` +10, `bridge.shadows_bytecode_anywhere` +1, `superseded.kind_disagreements` −2 | +6, via `RETIRED_SHADOW_TRIPLES` |
| (b) | the four new scalar `StringBuilder.insert` overloads (`IZ`/`IJ`/`IF`/`ID`) registered on `StringBuilder` / `StringBuffer` / `AbstractStringBuilder` — ambient kind `Bridge` | up to **+12** Bridge-over-bytecode rows, and the kind map gains twelve rows | **ZERO.** `Bridge` is not in that census's population |
| (c) | anything else in the 182 commits between the freeze and HEAD | unknown | unknown |

(a) and (b) are *arithmetic*, stated so the diff can be read, and neither is a
value anyone may paste. Every figure is a LOWER BOUND. If the run moves a
counter by anything else, that is (c) — a finding to attribute, not slack to
absorb.

**One retirement is deliberately NOT landed, and its consequence must not be
frozen for it.** The 7-row `java/io/Print*` shadow retirement in
`native-api/src/retired_shadow.rs` was verified to come back clean and was held
back, because landing it moves three `25/linux`-frozen artefacts —
`bridge_shadows_bytecode`, `stub_ratchet.rs`'s `SLACK = 0` baseline, and the
kind map — which must be re-frozen in the SAME commit from ONE census. Predicted
if it lands: `stub_ratchet` rises by up to seven in the (a) direction, and
`bridge.shadows_bytecode_anywhere` falls by the rows that leave the `Bridge`
population. Confirm with the same one command; do not pre-freeze for a change
that does not exist.

**And one census this script structurally cannot take.** `bridge-ratchet.sh`
runs `--real-jdk`, and the frozen artefact records `"mode": "compatible"`. A
registration reachable only from `register_synthetic_overrides` therefore
**cannot move it by any amount** — see
docs/architecture/natives-over-real-jdk-classes.md §7. Verifying a
synthetic-mode-only change (for instance, that the `ProcessBuilder` cluster's
three slots return to `SyntheticStub` with an empty `overwrote=`) needs a
`--dump-native-registry` diff from a `--features synthetic-jdk` binary run in
`--synthetic-jdk` MODE. Running this script for that question answers a
different one and answers it "nothing changed".

### (b) `cratonvm/internal/LinkedListSnapshotListItr` implements nothing

Carried over from `W7-16` because it is still true and still not this branch's
file. Measured in `Compatible` mode on the pre-built binary:
`listIterator() instanceof ListIterator` is `false`, and any erased-type
`(ListIterator) x` raises `ClassCastException`. `AbstractList.equals` never
trips it because its receiver is already typed `ListIterator`, so no
`checkcast` is emitted — which is why the family works at all.

One entry in `classloading/src/class_manager.rs`'s `jdk_interfaces`, beside
the `ArrayListSubList` line already there:

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

Independent of both halves above — it is a `Compatible` defect and lands on
its own. It becomes *more* urgent with this branch, not less: the carrier is
now reachable in strict mode too, so the missing interfaces are reachable in
both.

---

## Verification, once this is built

```sh
cargo build --release -p cratonvm-cli
for M in "--jdk-only" "--real-jdk"; do
  target/release/cratonvm $M --java-home "$JDK" -cp probes LaunderProbe
done
```

Expected: the two arms identical, and identical to `java`, on every row except
`ll.listIterator().getClass()`. In particular `al.equals(ll)` must be `true` in
both, and the strict run must **still** report `native-shadows-bytecode` for
`java/util/LinkedList.listIterator` under `--jdk-only-report`.

## Falsifying observations

* **If `al.equals(ll)` still answers `false` under `--jdk-only`** while
  `ll.equals(al)` succeeds, the error channel is not the mechanism and the
  comparison is short-circuiting somewhere above `collection_elements_generic`
  — check `al_eq_operand_is_list`, whose guard returns `Ok(Some(Value::Int(0)))`
  *before* either helper is called.
* **If the strict arm now raises where `Compatible` succeeds on a row that
  touches no `cratonvm/` class**, an `Ok`-with-nothing-usable arm was converted
  to `Err` by mistake. That is the one regression this change can cause, and
  every helper's `_ =>` arm is written to make it visible in review.
* **If `Compatible` reddens on a suite vector**, the likeliest cause is an
  exception that was previously being swallowed inside a foreign collection's
  own `iterator()`/`equals()`/`toString()`. That is a *found* defect, not a
  caused one — but it is a behaviour change, so attribute it by running the
  same vector on a binary without this branch before filing it here.
* **If `--dump-native-registry` still prints `synthetic-stub` for the nine**,
  the retag did not take: check that the entry is in sorted position, because
  `receiver_declared_by_no_supported_image` binary-searches and an unsorted
  entry fails silently in exactly this direction.

## Binary provenance

The pre-built binary used for every measurement predates `W2-1`'s iterator
fixes: it still raises `NoClassDefFoundError` for `java/util/ArrayDeque$Itr`
and `java/util/LinkedList$Itr` under `--jdk-only`, which that record fixed in
source on 2026-08-11. Those names appear in the strict column of the Part 1
table and are **not** findings of this record.

The check that establishes this rather than assuming it is `W7-16`'s and is
repeated here because it separates two symptoms that look alike: the `Bridge`
category those registrars set landed in `21d47faf5` (2026-06-02), long before
the binary, yet `--dump-native-registry` reports `synthetic-stub` — so the
downgrade is the `no_image_receiver` table and not the binary's age, while the
`NoClassDefFoundError`s *are* the binary's age.

## Probes

`LaunderProbe.java`, written for this record. Follows `W7-1`'s rule: every row
prints content, and each row catches per-member so one refusal does not
truncate the table. It deliberately includes four rows that were already loud
(`ll.equals(al)`, `hashCode`, `containsAll`, `listIterator`) beside the one
that was silent — a probe that only printed the failing row could not have
shown that the same refusal takes both shapes depending on the return type it
lands in, which is the entire diagnosis.
