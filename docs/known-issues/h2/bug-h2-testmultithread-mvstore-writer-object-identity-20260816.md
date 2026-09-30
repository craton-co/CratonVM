# `TestMultiThread` — MVStore background writer sees an object of the wrong class

## Status

> ### 2026-08-29 — THE FACE OF THIS DEFECT WILL HAVE CHANGED
>
> This page's verdict is a holder that keeps naming an address the ZGC slide
> vacated (`receiver names an address the ZGC slide VACATED …
> target_still_live=true`), and the three faces it records — `java.lang.Object`,
> a `java.math.BigDecimal`, a `java.lang.String` — are "whatever landed there".
>
> Until 2026-08-29 the usual answer was **nothing** landed there: the slide
> reclaimed only by dropping the bump cursor, so a vacated span under a pinned
> cursor was zeroed-or-stale memory that no allocator could hand out again. That
> leak was also the `OutOfMemoryError` behind
> `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`, and it is
> fixed: the span is now zeroed and returned to the free list
> (`CRATONVM_ZGC_PUBLISH_VACATED=0` reverts).
>
> So the stale read now meets a FRESH object far more often, and the
> distribution of faces on this page is stale evidence. Re-take it before
> reasoning from it, and use `CRATONVM_ZGC_PUBLISH_VACATED=0` as the arm that
> reproduces the old distribution.


**STILL OPEN 2026-08-21, but no longer unexplained.** **Twenty-one** real
defects behind it have been found and fixed — three in the reference machinery
(below), three in the stale-reference family the second pass went after (one of
which this page had filed as a separate curiosity), **three more of that same
family in the fifth pass, 2026-08-18** (see *Fifth pass*) including the one that
matches this page's own headline verdict, and **twelve in the sixth pass,
2026-08-21**, which finishes the `apps_h2.rs` worklist and leaves a test that
fails if a thirteenth is written (see *Sixth pass*). The failure itself still
reproduces, and the mechanism now has a measured name instead of four candidate
explanations.

**What the failure IS, measured:** a live object relocated by the ZGC slide,
with one holder never rewritten. The verdict comes out of the collector itself
now (see *Instruments added*):

```
receiver names an address the ZGC slide VACATED.  site="invoke dispatch"
  obj=0x20043d013f8  vacated_from=0x20043d013f8  moved_to=0x2004382e808
  original_class=org/h2/engine/SessionLocal  original_size=1056
  target_still_live=true
```

`target_still_live=true` is the whole finding: the object is alive at its new
address, so this is not a lifetime bug and not a reclamation bug. Something kept
naming the old address after the collector moved it, and the address has since
been re-occupied — which is why the receiver reads as `java.lang.Object`
(zeroed), as a `java.math.BigDecimal`, or as a `java.lang.String`, depending on
what landed there.

### What has been RULED OUT, each by measurement

| Hypothesis | Evidence against |
|---|---|
| the reference processor writing through a stale address | fixed (below), and the failure survives the fix |
| a heap reference slot the slide's rewrite pass missed | `CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`: `missed_rewrites=0`; the 35 unregistered targets are W7-84 primitives, `0 aliasing` |
| a parked thread's frame locals/fields going stale over `Thread.sleep` | `ParkedLocalProbe`: 8 holders x 400 rounds across 22304 relocated objects over 3 compactions — `bad=0`, same as HotSpot |
| a `long`/`double`-kinded local the remap refuses to rewrite | `CRATONVM_DBG_BUG03=1`: 5534 in-map local decisions, **every one** `skip_kind=false skip_nonobj=false` |
| a thread that no heal path reached | `thread_last_heal == heap_collection` at every reported stale slot |
| a waiter applying another pause's pointer map | `CRATONVM_DBG_MAPGEN=1`: **0** mismatches — every waiter gets the map of the pause it arrived for |
| a thread running Java while censused as blocked | `CRATONVM_DBG_BLOCKED_ACCESS=warn`: 0 violations |
| a live frame slot left naming a vacated address | 0, once the ledger stopped counting re-issued addresses — the 8-per-run the first version of that instrument reported were fresh allocations in the vacated span |
| a stale reference being STORED into a frame local | `set_local` detector: 0 hits across every reproduced failure |
| a stale reference being PUSHED on the operand stack | push detector (both `Value` and compact paths): 0 hits |
| a field read through a stale RECEIVER | `get_field` receiver detector: 0 hits |

### The stale-reference hunt (2026-08-17, second pass)

The address is not surviving the heal — it is **re-introduced after it**, by
code holding an `ObjectRef` in a Rust local where no root scan can see it. Three
defects of that family were found by pointing the (now exact) vacated-address
ledger at the forwarding barrier, and all three are fixed:

* **The forwarding barrier had nothing to read on this collector.**
  `VmHeap::load_and_forward` — the repair every one of its 46 call sites relies
  on, precisely because its caller holds a reference the collector cannot see —
  works by reading a FORWARDING WORD at the old address. ZGC's slide leaves
  none: `Arena::compact_low_to` zeroes the span above the new cursor and the
  memmove overwrites the rest. **The barrier was a silent no-op on the default
  collector.** The slide now publishes its `from -> to` pairs into
  `ZgcRealHeap::relocations` and the barrier consults them when the address is
  not a live object base (which is also why a re-issued address can never reach
  the table, so no pruning is needed for correctness).
* **`apps_h2::h2_comparison_compare` / `h2_comparison_get_value`** kept
  `session`, `left`, `right` and the two operand expressions in Rust locals
  across several `ctx` callbacks. Caught red-handed: with the ledger armed, a
  failing run logged the barrier being handed a moved address from exactly these
  two functions, and the failure it produced is the `SessionLocal` receiver at
  the top of this page. Now pinned and re-read.
* **`NativeContext`'s write entry points forwarded the RECEIVER but not the
  VALUE.** A native that read an object before a callback and stored it
  afterwards wrote a stale pointer straight into the heap
  (`properties_sidetable::mirror_loaded_entries_to_properties_backend` was
  caught doing it), where the next reader `checkcast`s it. `set_field`,
  `set_field_by_name`, `set_array_element` and `set_static_field` now forward
  the value too.

**The failure still reproduces**, so at least one producer of the same family is
still open: the surviving witnesses are `ClassCastException` at a `checkcast`,
i.e. a stale pointer read back out of a heap slot or read THROUGH a stale
receiver, with no `load_and_forward` on the path to catch it. The A/B over the
fix set is inconclusive at the rates measured (pre-fix 4 of 12 corrupt,
post-fix 3 of 8), which is exactly what one would expect if each fix removes one
producer out of several.

**What is no longer in doubt:** the residual is entirely a compaction problem.
`CRATONVM_ZGC_RELOCATE=0` is now **29 runs, 0 failures** (14 on the pre-fix
binary, 15 on the fixed one) against roughly a third of runs corrupt with
relocation on, under identical GC stress.

### Third pass: where the remaining producer is NOT

The two consumption points the barrier does not sit on were instrumented — the
operand-stack push (both the `Value` and the compact path) and the field-read
RECEIVER — and both report **zero** across every reproduced failure, alongside
`set_local`'s zero. So at the moment of the failure there is no reference to a
vacated-and-not-yet-reissued address anywhere in play.

That is not the absence of a defect; it is the instrument going quiet exactly
when the damage becomes visible. The exact ledger drops an address the instant
the allocator re-issues it (which is what makes it exact), and the failure only
becomes *observable* after re-issue: until then the stale holder reads the
zeroed corpse, and afterwards it reads a valid object of the wrong class. The
`checkcast` reporter, which consults the collector's own relocation history
rather than the ledger, still says what it always said:

```
receiver names an address the ZGC slide VACATED ... target_still_live=true
…and this is where that address stood in the OWNING thread's own GC
   bookkeeping. in_published_snapshot=false — the snapshot the collector marks
   this thread from did not contain a slot the thread's frames hold: a root
   COLLECTION gap, not a mark or sweep one.
```

**The one lever that moves it:** `CRATONVM_NO_LOCAL_LIVENESS=1` — the kill
switch for the per-bci local-liveness root filter. Interleaved ABBA with the
detectors armed on both arms:

| arm | runs | corrupt |
|---|---|---|
| A — per-bci liveness ON (default) | 12 | **5** |
| B — `CRATONVM_NO_LOCAL_LIVENESS=1` | 8 | **0** |

**Read that as masking, not as the culprit.** The filter's contract is
per-instruction bytecode liveness, and the slot it was caught dropping —
`MVPrimaryIndex.lockRow pc=22 local[2]`, traced by recording every address the
filter withholds and looking the failing receiver up in it — is the `Row`
parameter at the method's `areturn`, which is genuinely dead: nothing reads it
again. What the filter changes is how quickly a dead object's address becomes
**re-issuable**, and re-issue is what turns a latent stale holder into a
`ClassCastException`. Retaining every dead local hides the defect by keeping the
address occupied by the right object.

### Fourth pass: the root inventory is clean, and one near-miss

Two more instruments, and a fix that was nearly landed on an artifact.

* **The two in-pause frame verifiers were reading a forwarding word ZGC never
  writes.** `ARRIVE-STALE` and `WAKE-STALE` asked `debug_forwarded_target`, so
  both reported zero on the default collector whatever the truth was. Each now
  uses the record it already holds — this collection's `pointer_map` at the
  arrival site, the accumulated `fixup` chain at the wake site. These are the
  only EXACT places to ask: the remap has just run and no mutator on the thread
  has resumed, so a slot holding a map key is unambiguously one the remap did
  not reach. **Result: 0 across 23 runs**, once slide DESTINATIONS are excluded
  (before that exclusion it "found" 80-168 a run, every one a slot legitimately
  holding the survivor that slid INTO a vacated address).
* **`CRATONVM_DBG_ROOT_REMAP_AUDIT=1`** re-runs the root scan at the end of
  `update_all_roots` and looks for an address this collection moved. The scan
  inventory (`roots.rs`) and the remap inventory (`native_roots.rs`) are two
  different lists, and a source in the first but not the second is exactly this
  defect's shape. `collect_roots` is now labelled by section, so a hit names
  which of its forty sections produced the root. **Result: 0.**

**The near-miss, recorded because it nearly shipped.** Placed BEFORE the
blocked-thread fold, the audit reported 387-407 un-remapped roots a run, every
one from section 11, "Root snapshot (for cross-thread GC scanning)" — which
reads exactly like "the fold skips non-blocked threads, so their published
snapshots are never rewritten". A fix for that was written, and then the control
(`CRATONVM_NO_UNBLOCKED_SNAPSHOT_REMAP=1`, one binary, audit at the END) reported
**0 with the fix and 0 without it**: every stale snapshot entry belonged to a
BLOCKED thread and the existing fold already handled it. The 387 were an artifact
of where the audit ran, not a finding. The change was reverted and the reasoning
left in `fold_pointer_map_into_blocked_audited` for whoever measures the
excluded-thread race for real.

### Where the reference must therefore be

Heap slots (slide verifier), frame locals and stacks (both heal sites, exact
predicates), and the entire scanned root inventory are each verified complete at
the end of the pause. So the holder is none of them: it is a raw `ObjectRef` in
VM-side state that is **neither scanned nor remapped** — a native's Rust local
across a callback, or a side table in neither inventory. That is the same class
the barrier backtraces caught twice already (`apps_h2`,
`properties_sidetable`), and the `load_and_forward` instrument from the second
pass is the one that names them, one at a time, as each is fixed.

### Fifth pass (2026-08-18): three more producers of that class, named and fixed

The prediction above held. Pointing `CRATONVM_DBG_VACATED_FRAMES=1` at the
`MvidRepro` loop again produced a barrier backtrace naming a **third** `apps_h2`
native, and reading its neighbours found two more of exactly the same shape. All
three are fixed:

* **`h2_parser_test_token_fast`** — the one the instrument caught. `this` was
  never pinned at all and is read (`identifiersToUpper`) after
  `asIdentifier()`; `expected` and `token` *were* pinned, but re-read into
  bindings declared **inside the match arm**, so the repaired values died with
  the arm's scope and the stale outer `expected` was what reached
  `native_string_equals`; and `identifier` was live across the read below it.
* **`h2_condition_and_or_get_value`** — `this` read after the first `getValue`
  callback, and **`session` passed as an argument to the second**. This is this
  page's own headline verdict (`original_class=org/h2/engine/SessionLocal`,
  `site="invoke dispatch"`): a `SessionLocal` reaching `getValue(SessionLocal)`
  after a relocation. Both operands are also compared by identity against
  `ValueNull.INSTANCE` after a callback.
* **`h2_coalesce_function_get_value`** — the same, once per loop iteration, with
  `args`, `type`, `session` **and** `ValueNull.INSTANCE` live across the
  callback. A stale `INSTANCE` compares unequal to everything, so COALESCE would
  return its first argument instead of skipping NULLs.

**The rule these share, worth stating once:** the receiver of a `ctx` call is
repaired by `load_and_forward`; an **argument** handed to `ctx.invoke_*` is not.
A native may hold a raw `ObjectRef` across its own Rust code — the STW census
waits for `NativeRunning` precisely so the collector cannot move under it — but
**a callback into Java ends that protection**, and everything the native still
holds must be pinned across it and re-read afterwards, into the OUTER binding.

**A/B, ABBA-interleaved, 16 runs per arm, one binary per arm:** pre **3/16**,
post **2/16**. Indistinguishable — which is what this page already predicts for
removing one producer out of several. The fixes are justified by being provable
memory-safety defects under the VM's own stated rule, not by this number.

### Do not misread `interior_off` in the verdict

Every verdict reproduced in this pass carried a **non-zero `interior_off`**
(88 into a 176-byte `CacheLongKeyLIRS$Entry`, 72 into a 96-byte `BigDecimal`,
48 into an 80-byte `SimpleRowValue`, 8 into a 128-byte `IndexCondition`). That
reads like an interior-pointer defect and is not one. `zgc_corpse_lookup` maps
the address into the *extent of a previously vacated object*, so a non-zero
offset only says the vacated span was later re-issued at **sub-object
granularity** — a `String` or a `byte[]` now legitimately starts partway into
what used to be one bigger object. It is the re-served face this page already
describes, restated by a different instrument. (Checked against
`vm/src/memory/reclaim_guard.rs` before acting on it.)

### The remaining worklist, made concrete

> **CLOSED 2026-08-21 by the sixth pass below.** The table is kept as the
> record of what was outstanding; every row in it has been read, and the
> `pin_audit_witness` test now fails if a new one appears. The sentence after
> the table — that the same audit is owed by every OTHER native that calls back
> into Java — is still open.

`apps_h2.rs` alone has **15 more natives that call back into Java and pin
nothing**. Not every one is a defect — a function that touches no reference
after its callback is fine — but each has to be read, and all three fixed above
came out of this list. Ordered by number of callbacks:

| native | line | callbacks |
|---|---:|---:|
| `h2_cardinality_expression_get_value` | 1052 | 12 |
| `table_filter_prepare_on` | 3181 | 11 |
| `h2_constraint_run_existing_data_query` | 2346 | 8 |
| `h2_constraint_check_existing_data` | 2246 | 3 |
| `h2_constraint_check_column_types` | 2312 | 3 |
| `h2_internal_error` | 605 | 2 |
| `h2_invalid_array_value` | 1030 | 2 |
| `h2_sql_fragment` | 2537 | 2 |
| `h2_read_string_hash` · `h2_value_is_false` · `h2_default_row_get_value` · `h2_boxed_long_value` · `h2_parser_read` · `h2_syntax_error` · `h2_db_exception` | — | 1 each |

Regenerate it by scanning for `fn`s that contain `ctx.invoke_*` and no
`pin_native_root`. **And `apps_h2.rs` is one file** — the same audit is owed by
every native that calls back into Java.

**Next step, and a correction to the one this page used to carry.** The previous
"next step" — keep the full vacated history and disambiguate with the identity
hash — does not work as stated. For an address that has been re-issued, "the
identity here differs from the one recorded at the move" is equally true of a
**legitimate** holder of the new object, so the predicate over-reports instead of
naming the stale holder. It becomes sound only when scoped by *when the holder
obtained the reference* — e.g. stamping each native call with the collection
count at entry and flagging only a reference to an address vacated since. The
cheaper route is the one that worked twice more this pass: run the barrier
instrument and work the table above.

One instrument was added for the gap that is still genuinely blind — the window
BEFORE re-issue, where a stale holder reads a zeroed corpse and
`class_id_of`/`kind_of` swallow it into `ClassId(0)`/`ObjectKind::Object` with
nothing thrown. `VmHeap::note_dead_base_deref` reports that swallow with a
backtrace under the same flag. **It has not fired on any reproduced failure
yet**, which says the stale reads seen here all land on re-issued memory rather
than on the corpse. Recorded as an untriggered instrument, not as evidence.

### Sixth pass (2026-08-21): the `apps_h2.rs` worklist is finished, and the file now ratchets

The *remaining worklist, made concrete* table above is closed. Every native in
`apps_h2.rs` that can move the heap has been read; twelve were repaired and the
rest are named, with the reason each cannot go stale, in a test that fails if a
thirteenth is written.

**First, what changed under the worklist while it sat there.**
`vm_exec.rs::forward_boundary_args` landed after the fifth pass and forwards
every reference ARGUMENT a native hands to `invoke_*`, alongside the
receiver-forwarding every `NativeContext` entry point already did and the
value-forwarding the write entry points gained in the second pass. That removes
the worst outcome — a stale pointer escaping into a Java frame — from every one
of these sites at once, and its own doc says plainly that it *"does not make
per-site pinning unnecessary."* It is also best-effort: `load_and_forward`
repairs an address only while the collector still has a record of the move, and
a re-issued address never reaches that table.

So the residue this pass had to work is what **no** repair path covers:

| shape | repaired by the boundary? |
|---|---|
| an `ObjectRef` compared by **identity** in Rust (`x == INSTANCE`, `nested != this`) | **no** — nothing on the path even sees it |
| `ctx.class_id_of_object(x)` / `h2_class_name(x)` | **no** — `class_id_of` with no `load_and_forward` in front |
| `ctx.read_string(x)` | **no** — resolves the class off the raw address |
| receiver of any `ctx` call | yes (`load_and_forward`) |
| argument to `ctx.invoke_*` | yes (`forward_boundary_args`) |
| value stored by `set_field*` / `set_array_element` | yes (`forward_boundary_value`) |
| `ctx.array_length` / `ctx.get_array_element` | yes (`load_and_forward`) |

**The twelve.** Ordered by how bad a stale read is, not by file order.

* **`h2_cardinality_expression_get_value`** — `ValueNull.INSTANCE` is read
  before `arg.getValue(session)` and compared by IDENTITY after it. This is the
  COALESCE shape from the fifth pass exactly: a stale `INSTANCE` compares
  unequal to everything, so `CARDINALITY(NULL)` falls through to the type switch
  instead of returning NULL. `session` is also passed as an argument after two
  callbacks, and `value` is read RAW (`class_id_of_object`) after `getValueType`.
* **`h2_value_is_false`** — `value` is compared by identity against
  `ValueNull.INSTANCE` and `ValueBoolean.FALSE`, both obtained through
  `h2_static_object`, which runs `<clinit>` the first time it sees the class.
  A stale `value` matches neither and the predicate then answers from
  `getBoolean()` on whatever now occupies the address.
* **`table_filter_prepare_on`** — `this` is compared by identity against
  `nestedJoin` and `join`; that comparison is the self-join guard that
  terminates the recursion. `col` is an argument to `getColumnIndex` after
  `getColumnId`, `session` is passed to the SECOND `optimizeCondition` after the
  first has run, and `conds` / `index` are live across the whole pruning loop.
* **`h2_constraint_run_existing_data_query`** — `this` is an argument to
  `getShortDescription` after the entire prepare / query / close chain;
  `sql_obj` is an argument to `Session.prepare` after
  `startStatementWithinTransaction`; both `IndexColumn[]`s are read element-wise
  inside the SQL builders.
* **`h2_constraint_check_column_types`** — its doc comment argued no pin was
  needed, and the argument was right about the window it considered (`getType()`
  is a plain field getter, so nothing moves between reading the two `TypeInfo`s
  and passing them). It did not consider the LOOP: `checkComparable` allocates,
  so from the second iteration onward both arrays and `this` are held across a
  mover. A premise-scoped guard is only as good as its premise, and the comment
  now says which premise it is.
* **`h2_constraint_check_existing_data`** — `session` is an argument to
  `Table.getRowCount` after `getDatabase`, `isStarting` and the whole column-type
  check; both refs are then handed to the query runner.
* **`h2_index_columns_sql`, `h2_index_columns_is_not_null`,
  `h2_index_column_join_sql`** — each walks an `IndexColumn[]` calling
  `h2_sql_fragment` per element, which allocates a `StringBuilder` and calls
  back into Java twice. The loop-carried array binding is **reassigned**, not
  shadowed: a `let` inside the loop body dies with the iteration and the next
  one would read the stale outer copy again, which is the mistake
  `h2_parser_test_token_fast` was fixed for in the fifth pass.
* **`h2_sql_fragment`** — `obj` is live across the `StringBuilder` allocation and
  `sb` across `getSQL`. Both are receivers, so both are repaired today; pinned
  anyway, because that repair had no forwarding word to read on the default
  collector until 2026-08-17 and a pin costs nothing here.
* **`h2_invalid_array_value`** — `trace_sql` is live across `create_string` and
  is then an argument.
* **`h2_db_exception`** — `arg_array` is live across a `create_string` per
  message argument and is then an argument itself.
* **`h2_parser_read`** — `s_obj` is live across `native_string_length` and is
  then handed to `ctx.read_string`, one of the few `ctx` readers with no
  `load_and_forward` in front of it.
* **`h2_long_data_type_binary_search`** — `storage` is live across
  `h2_boxed_long_value`, which falls back to a `Long.longValue()` callback when
  the box is not laid out as expected. MVStore drives this per key.

**Second, the file now ratchets.** `apps_h2::pin_audit_witness` reads
`apps_h2.rs` with `include_str!` and fails if any top-level function that calls
a heap-moving `ctx` operation pins nothing, unless it is named in
`JUSTIFIED_UNPINNED` with the reason it cannot go stale. Seventeen are named
there — "receiver-only", "hands its argument straight to the callee", "the
string it creates is the very next call's argument". A companion test fails if
that list names a function the file no longer defines, because a stale exception
list is how an audit stops auditing, and a third feeds the same predicate a
synthetic offender and a synthetic compliant function so a green cannot mean
"the scan looks at nothing".

This is deliberately a source witness rather than a behavioural test.
`MockNativeContext` never relocates, so a mock-driven test would assert that a
pin was *called*, not that it *helped* — and the fifth pass's `apps_h2` bugs
were all found by reading the source with the barrier instrument pointed at it,
not by a unit test.

**What this does NOT claim.** The failure at the top of this page is not
retired. The audit closes `apps_h2.rs`; the page's own next line — *"and
`apps_h2.rs` is one file — the same audit is owed by every native that calls
back into Java"* — is untouched, and the same scan over the other
`native-builtins` files is the next tranche. No A/B is quoted for this pass
either: at the rates this page has already measured (pre 3/16, post 2/16 for
three producers), twelve more sites removed from one file cannot be separated
from noise in any run budget available here. They are justified the way the
fifth pass's were — as provable violations of the VM's own stated rule, which
`NativeContext::pin_native_root` writes down.

**Verified:** `cargo test -p cratonvm-native-builtins --lib apps_h2` (9/9,
including the three witnesses); an 18-class H2 regression set spanning the
constraint, table-filter, view, index and parser natives — 16 PASS / 2 FAIL
before and after, the two being `TestBnf`/`TestWeb`, which fail on `dev` for the
unrelated `Sentence.MAX_PROCESSING_TIME` reason their own page records.

## The three reference-machinery defects fixed on the way

### 1. `Collections.synchronizedSet` / `synchronizedMap` / `synchronizedList` never took their `mutex`

The natives forwarded straight to the backing collection; the `mutex` field was
written by the constructor and read by nobody. A registered native shadows the
class's own bytecode at every dispatch site, so the real JDK implementation
could not compensate — and `SynchronizedList`, which has no natives of its own,
inherits `add`/`remove`/`size` from `SynchronizedCollection` and lost elements
too. 8 threads x 4000 distinct elements added then removed (`SyncSetProbe`):

```
                HOTSPOT   BEFORE   AFTER
  set.size()       0       3716      0
  map.size()       0       1888      0
  list.size()    32000     26540   32000
```

`3716` is this page's own `size() == -26`, which it filed as "probably a
separate defect worth its own look". It was not separate: H2 keeps
`CloseWatcher.refs` (a set of `PhantomReference`s, one per connection) in one of
these wrappers, and losing entries from it kills a `Reference` the VM's
processor is still tracking. Isolated with `PhantomIdentityProbe -Dsyncset=true`
(3200 phantom references through one queue on 8 threads; `delivered` = polled
back out):

```
  HOTSPOT   3200 3200 3200
  BEFORE    2616 3106 3189 3184
  AFTER     3200 3200 3200 3200
```

With a `ConcurrentHashMap`-backed registry instead, both builds deliver cleanly
— which is what identifies the wrapper rather than the reference processor.

Contended entry is `monitor_enter_gc_safe`, not `monitor_enter`: these wrappers
are contended by construction and the owner is inside `HashMap.put`, which
allocates and can be parked at a safepoint holding the mutex — a plain
`monitor_enter` leaves the waiter counted in the STW barrier's `expected` set,
which is the three-way wedge `Monitor::block_enter` documents. The GC-safe wait
can span a moving collection, so `this`, the mutex and every reference argument
are pinned across it and re-read afterwards.

### 2. The pre-GC referent-null pass was the one unscreened reference-processor write

`process_references_after_gc`'s cleared / enqueue / restore loops were given a
class-shape guard on 2026-08-16. `weakref_null_referents_pre_gc`, which performs
the same kind of write through the same kind of address, kept only
`num_fields >= 2` — which `org.h2.engine.SessionLocal` passes, and every
`org.h2.value.Value` passes. That is why this page could record the failure as
surviving "the shape guard that now screens every reference-processor write": it
did, because this write was not one of the screened ones.

### 3. The same-class hole is closed by an identity stamp

A shape guard cannot tell a reclaimed `Reference` from ANOTHER `Reference`
re-issued at the same address, and H2 allocates a `CloseWatcher` per connection,
so same-class re-issue is the common case rather than the exotic one. Every
entry now carries the identity hash the object had at `discover_reference` time
(`VmHeap::identity_hash_code` mints from a monotonic counter into the object's
own mark word, and the mark word travels with the object across a relocation —
the "monotonic registration id written into the object" this page's *Next steps*
asked for, already present). Checked at the pre-GC null pass and all three
post-GC write sites. Both "cannot tell" answers — an unstamped entry, and a
thin-locked object whose hash is displaced out of the mark word — fall back to
the shape guard rather than declining, so the stamp can refuse a write but can
never lose a legitimate one. Verified: `PhantomIdentityProbe` delivers exactly as
before the stamp (3110-3200 of 3200, unchanged distribution) with zero
`SKIP reidentified`.

## Instruments added (each of them answered something this page could not)

* **`VmHeap::reclaimed_hole_at` / `live_holders_of` now have ZGC arms.** They
  answered `None`/empty on the DEFAULT collector since 2026-08-10, so the whole
  flag-free reclaimed-receiver verdict in `vm/src/memory/reclaim_guard.rs` was
  inert exactly where this defect lives: a reproduced failure logged 13
  `gc::guard` lines, all of them unrelated startup warnings.
* **`report_reclaimed_receiver` consults ZGC's relocation ledger** when
  `CRATONVM_DBG_ZGC_CORPSE` armed the run. That is what produced the verdict at
  the top of this page.
* **The last un-migrated copy of that verdict — `checkcast` — now calls the
  shared reporter.** The re-served face of this defect has a non-zero class id,
  which is exactly what the local copy's `ClassId(0)` gate suppressed.
* **`CRATONVM_DBG_VACATED_FRAMES=1`** records one collection's pointer-map keys
  and reports any LIVE frame slot still naming one at the next safepoint, with
  thread, method, pc, slot, the class now at the address, and where the original
  went. An address that is also a slide DESTINATION is excluded — survivors
  slide onto vacated addresses, and reporting those manufactures findings (the
  first version of this instrument did, on `ThreadPoolExecutor.runWorker`).
* **`CRATONVM_DBG_MAPGEN=1`** checks that a safepoint waiter gets the pointer map
  of the pause it arrived for — the release condition and the map read are two
  different facts. It reports 0, which retires that hypothesis.

## Repro

```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

Roughly 1 run in 4 on a loaded host. The extracted `testConcurrentUpdate`
(`MvidRepro`: 25 connections x 1000 committed `UPDATE`s over a 10000-row table)
plus `CRATONVM_DBG_GC_STRESS=4194304 -Dupdates=120` is the same failure in ~70 s
with 80 collections and 72 compactions per run instead of 2 — measured about 1
in 15 that way, against >900 s for the whole class to reach the method at all.
**`rc=0` is not a pass**: the MVStore background writer's panic does not fail the
main thread, so grep the stderr:

```bash
grep -cE "Exception in thread|NoSuchMethodError|ClassCastException|Cannot invoke" err.log
```

Two greps discriminate it from the retired reference-queue bug, and both must be
empty:

```bash
grep -c "cannot be cast to class org.h2.util.CloseWatcher" err.log   # retired bug
grep -c "Cannot read the array length"                     err.log   # retired bug
```

## The compaction correlation

Interleaved ABBA on the pre-fix binary: ZGC relocation ON, **2 of 6** runs FAIL;
relocation OFF (`CRATONVM_ZGC_RELOCATE=0`), **0 of 14**. Compaction is what
re-issues a vacated address quickly enough for the stale holder to reach a live
object of another class, so it is the amplifier — and, on the evidence above,
also the necessary condition. `CRATONVM_ZGC_RELOCATE=0` is therefore a usable
mitigation for a workload that hits this, at the cost of the only
defragmentation this collector has.

## Original witnesses (filed 2026-08-16, unchanged)

Both are the MVStore **background writer** thread committing
`.../data/test/lockMode.mv.db`, inside `TestMultiThread.testConcurrentUpdate`,
`--nojit`, `--Xmx 1g`:

```
[cratonvm] WARN vm_exec: NoSuchMethodError
    method="java/lang/String.toByteArray()[B"
    caller="org/h2/mvstore/db/ValueDataType.write(Lorg/h2/mvstore/WriteBuffer;Lorg/h2/value/Value;)V @pc=529"
```

```
Exception in thread "MVStore background writer .../lockMode.mv.db"
  ... java.lang.NullPointerException: Cannot invoke
  "org.h2.value.Value.getValueType()" because "v" is null [2.4.249/3]
```

Four more faces were measured during this work, all the same mechanism:

```
NoSuchMethodError: 'int java.lang.Object.compareWithNull(
    org.h2.value.Value, org.h2.value.Value, boolean)'   <- receiver was a SessionLocal
ClassCastException: [Lorg.h2.value.Value;      cannot be cast to org.h2.mvstore.Page
ClassCastException: java.math.BigDecimal       cannot be cast to org.h2.mvstore.Page
ClassCastException: java.lang.ref.WeakReference cannot be cast to ...
```

It is present on pristine `origin/dev` as well as on every build since; on the
pristine arm it was usually pre-empted by the louder reference-queue bug
(retired 2026-08-16), which is why it had not been seen alone before. HotSpot
JDK 25 passes the same class on the same classpath in the same sessions.
