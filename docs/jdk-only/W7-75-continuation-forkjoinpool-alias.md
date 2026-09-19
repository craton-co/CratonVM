# W7-75 — the two unguarded LIVE read-side aliases: `Continuation` and `ForkJoinPool`

> ## RE-VERIFIED 2026-08-12 (lane A14). Both layout censuses reproduce independently. The repair is in the tree. Still unrun.
>
> §1 says its tables were re-derived rather than copied, and that four censuses
> in this area were each wrong about some rows on the same day. So they were
> re-derived a third time, on a **different vendor's** JDK 25 — Microsoft
> OpenJDK 25.0.3.9, this host's `java.home`, where §1 used Adoptium — with
> `javap -p`, statics excluded, in declaration order.
>
> **Both tables are correct in every row.**
>
> * `jdk/internal/vm/Continuation`: `target scope parent child tail done
>   mounted yieldInfo preempted scopedValueCache` at 0–9. So the synthetic
>   map's swapped `scope`/`target`, its `state` landing on `parent`, and its
>   `pin` landing on `child` all reproduce exactly as §1 and §3.1 state.
> * `java/util/concurrent/ForkJoinPool`: sixteen instance fields,
>   `termination` at 0, `saturate` at 1, **`parallelism` at 15**.
> * `java.util.concurrent.AbstractExecutorService` declares **no instance
>   field** — its only member is the static `$assertionsDisabled`. This is the
>   number §1 flags as "the one a careless count would get wrong", and it is
>   right: ForkJoinPool's own sixteen are the whole chain, so `parallelism` is
>   15 and not 15-plus-something.
>
> **The repair is present**, checked by symbol rather than line (every line
> number in this record has since drifted): `ContSlots` / `cont_slots`,
> `FjpSlots` / `fjp_slots`, `NEW15_CONT_SLOT_MAP` / `NEW15_FJP_SLOT_MAP`, both
> `read_alias::declare_slot_map` calls, and the named unit tests
> (`cont_slots_resolves_the_real_layout_by_name`,
> `cont_slots_falls_back_to_the_synthetic_map_on_an_anonymous_class`,
> `fjp_slots_falls_back_to_the_synthetic_map_on_an_anonymous_class`) are all
> in `native-builtins/src/phases_late/concurrent.rs`.
>
> **§2's liveness verdict reproduces**, which matters because it is what makes
> the `ForkJoinPool` half a live wrong answer rather than dead code. The
> real-ForkJoinPool filter's `keep_real_forkjoinpool_bridge` in
> `native-api/src/registry.rs` still begins
> `self.effective_category() == NativeKind::Bridge` and its match list contains
> `("getParallelism", "()I")`, `commonPool`, `getCommonPoolParallelism` and
> `getFactory` — and **still does not contain `getActiveThreadCount`**, so §8.3
> holds: that one is dropped on the default path. `is_forkjoin_native_override`
> in `vm/src/runtime/interpreter/native_override.rs` also lists
> `("getParallelism", "()I")`, so the native is forced ahead of real bytecode
> at every dispatch site, exactly as §2 says.
>
> **Nothing is refuted and nothing is discharged.** §8.5 is still the operative
> line: the probe's CratonVM column is unrun and this lane cannot run it. What
> this pass adds is that the two censuses the whole repair rests on now have
> two independent derivations on two vendors' JDK 25 images, which is the part
> that was most exposed to the "four censuses were each wrong" hazard.
>
> **One caution for whoever runs the probe**, stated because it is the shape
> that has produced false REDs elsewhere in this campaign: the common pool's
> parallelism **legitimately** differs between the two VMs. CratonVM clamps it
> to 1 for determinism (`NEW15_COMMON_POOL_PARALLELISM`); HotSpot uses
> `max(1, availableProcessors() - 1)`, which is 31 in §5.1's transcript and
> will be whatever this host reports elsewhere. §5.1 already says the
> comparable assertion is `agree=true` and not the value — that is not a
> nicety, it is the difference between reading a green run and filing a
> phantom defect. The same rule forbids ever printing that number from a
> `regression-suite` fixture, since `run.sh` diffs the two runs' `CK` lines in
> one session.
>
> **New, small, and filed rather than fixed:** `class_manager.rs` fabricates
> `"java/util/concurrent/ForkJoinPool" => instance_fields(1)` — width **one** —
> while `NEW15_FJP_SLOT_MAP` declares **two** synthetic slots
> (`parallelism`=0, `active`=1). On a fabricated receiver that reaches the
> fallback, slot 1 is past the declared width. §3.2 notes `commonPool()`
> allocates through `try_alloc_concurrent_synthetic`, which clamps up to the
> loaded class's width, so on the real-JDK path the object is genuinely 16
> slots and this cannot bite; the exposure is the synthetic path, which is also
> where `active` is only reachable under `CRATONVM_SYNTHETIC_FORKJOINPOOL`
> (§8.3). Recorded so the next reader of the published map does not have to
> re-derive it; not fixed, because it needs the run §8.5 wants and it touches
> a file this lane does not own.

Status: both repaired by resolving the fields by NAME on the receiver's own
class, with the legacy slot map kept as the fallback and **published** through
`cratonvm_native_api::read_alias::declare_slot_map`. The completed-continuation
guard W7-69 §6(1) records as never firing now fires, and
`ForkJoinPool.getParallelism()` on a pool the native did not allocate answers the
pool's own parallelism instead of a fallback constant.

**Nothing here was built or run.** This lane may not invoke `cargo`; the
orchestrator builds. Every claim about the real JDK is `javap -p` or a `java` run
against Eclipse Adoptium 25.0.3.9 on this Windows host and says which. Every
claim about CratonVM is source-level and says so. The three gates of
`native-api/tests/read_alias_coverage.rs` this change is exposed to were
re-implemented outside the tree and run green against it and red against their
own mutations (§7).

Branch `fix/continuation-forkjoinpool-slot-alias-20260812`.

## 1. The layouts, re-derived

`javap -p` against Adoptium 25.0.3.9 (`javap -version` → 25.0.3), transitive over
the superclass chain, `static` excluded — the convention
W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md,
W7-59-layout-detector-coverage.md and W7-69-read-side-alias-instrument.md all
use.

| idx | `jdk/internal/vm/Continuation` (superclass `java/lang/Object`) | the synthetic map calls it |
|---:|---|---|
| 0 | `target` `Runnable` | **`scope`** |
| 1 | `scope` `ContinuationScope` | **`target`** |
| 2 | `parent` `Continuation` | **`state`** (an `int`) |
| 3 | `child` `Continuation` | **`pin`** (an `int`) |
| 4 | `tail` `StackChunk` | **`preempted`** (an `int`) |
| 5 | `done` `boolean` | — |
| 6 | `mounted` `volatile boolean` | — |
| 7 | `yieldInfo` `Object` | — |
| 8 | `preempted` `boolean` | — |
| 9 | `scopedValueCache` `Object[]` | — |

| idx | `java/util/concurrent/ForkJoinPool` | the synthetic map calls it |
|---:|---|---|
| 0 | `termination` `CountDownLatch` | **`parallelism`** (an `int`) |
| 1 | `saturate` `Predicate` | **`active`** (an `int`) |
| 2–14 | `factory ueh container workerNamePrefix poolName delayScheduler queues runState keepAlive config stealCount threadIds ctl` | — |
| 15 | **`parallelism` `int`** | — |

`ForkJoinPool`'s superclass `java.util.concurrent.AbstractExecutorService`
declares **no instance field** — its only member is the static
`$assertionsDisabled` — so ForkJoinPool's own sixteen are the whole chain. That
is what makes `parallelism` index 15 and not 15-plus-something, and it is the one
number in this record that a careless count would get wrong.

**Disagreements with W7-69: none.** Its §6(1) and §6(2) tables reproduce exactly,
including the swapped `scope`/`target` pair, `state` landing on `parent`, and
both FJP slots being references. Four censuses in this area were each wrong about
some rows on 2026-08-12; this one is not, and it was re-derived rather than
copied. `jdk/internal/vm/ContinuationScope`'s one-slot map (`name` at 0) also
reproduces as clean — it is in W7-69 §4.4's list of 23 that agree, and it stays
there.

Both tables are now pinned in code as `REAL_CONT_FIELDS` / `REAL_FJP_FIELDS`
(`native-builtins/src/phases_late/concurrent.rs`, `new15_tests`), so the next
reader diffs against data rather than against prose.

## 2. Liveness: which registrar wins, and how that was decided

Not by indentation, and not by brace-scanning by hand. The function-span scanner
used here was **validated against six ground-truth anchors first** — including
`register_builtins` at `native-builtins/src/lib.rs:21401-21406`, whose bounds a
plain column-0 grep also gives — before any verdict was taken from it. That
mattered: `native-builtins/src/lib.rs` contains **column-0 `}` closers at 11604
and 20415 that close NESTED helper functions**, not the enclosing registrar, and
a scanner that trusts them puts `register_new15_loom`'s call site in the wrong
function. This is the third lane in this campaign to meet that trap.

**`jdk/internal/vm/Continuation` — one registrar, no contest.**
`register_new15_continuation` (`concurrent.rs`) is the only place in the
workspace that registers on that class name; grepping the literal
`"jdk/internal/vm/Continuation"` across every crate finds it, its own tests, and
nothing else. It is reached from `register_new15_loom`, called at

* `native-builtins/src/lib.rs:20539`, inside
  `register_essential_natives_with_shims` (`7084`–`20934`) — the real-JDK arm,
  which `vm/src/vm/vm_init.rs` calls at `:1960` (synthetic-jdk feature build,
  `else` of `if config.use_synthetic_jdk`) and `:2498` (default feature build);
* `native-builtins/src/lib.rs:23962`, inside `register_synthetic_overrides`
  (`21412`–`24193`) — the synthetic arm.

LIVE in both modes.

**`java/util/concurrent/ForkJoinPool.getParallelism()I` and `commonPool()` —
three registrars, the last one wins, and a filter drops most of the field.**
Registration order on the real-JDK path, all inside
`register_essential_natives_with_shims`:

| line | registrar | registers `getParallelism` |
|---:|---|---|
| 10203 | `phases_early::register_real_jdk_forkjoin_essentials` | yes (`phases_early.rs:10231`) |
| 10222 | `register_t19_k3_forkjoinpool_common` → `register_new15_forkjoinpool_common` | yes |
| 20539 | `register_new15_loom` → `register_new15_forkjoinpool_common` | yes |

Registration is last-write-wins and re-registering a triple updates the slot in
place, so **`register_new15_forkjoinpool_common` is the winner** — the function
this lane edited. On the synthetic path the same registrar wins at
`lib.rs:23962`; `concurrent_extras::register_forkjoin_extras`, which runs
immediately after at `:23968`, registers no `getParallelism`.

`native-builtins/src/jmx.rs:5580` also registers a `getParallelism()I`, on
`jdk/management/VirtualThreadSchedulerMXBean` — a different class, not a
competitor.

**`NativeKind` is ambient and here it is `Bridge`, which is load-bearing twice.**
`register_new15_forkjoinpool_common` and `register_new15_continuation` each open
with `r.set_category(NativeKind::Bridge)` and close with
`r.set_category(__prev_cat)`; every registration in between is a plain
`r.register(..)`, so `effective_category()` is `Bridge`. Both consumers of that
demand exactly `Bridge`:

1. `native-api/src/registry.rs`'s real-ForkJoinPool filter —
   `if real_forkjoinpool_enabled() && class_name == ".../ForkJoinPool" &&
   method_name != "execute" && !keep_real_forkjoinpool_bridge { return; }` —
   where `keep_real_forkjoinpool_bridge` begins
   `self.effective_category() == NativeKind::Bridge`. `real_forkjoinpool` is
   `!present(CRATONVM_SYNTHETIC_FORKJOINPOOL)` (`types/src/flags.rs:1902`), i.e.
   **on by default**. A registration tagged anything else would be silently
   dropped — the inert-fix shape seven lanes shipped on 2026-08-12.
2. `vm/src/runtime/interpreter/native_override.rs`'s
   `synthetic_stub_kind_should_yield_to_real_bytecode`, whose first line is
   `if kind != Some(NativeKind::SyntheticStub) { return false; }`. A `Bridge`
   native therefore does **not** yield to the class's real bytecode, which is
   what makes these natives run at all on a real `Continuation` / `ForkJoinPool`.

**`getActiveThreadCount()I` is NOT on that keep list**, so its registration is
dropped on the default path and the `NEW15_FJP_ACTIVE` read never runs there.
`getParallelism`, `commonPool`, `getCommonPoolParallelism` and `getFactory` are
all on it. `getParallelism` is additionally on
`is_forkjoin_native_override`'s list, so `force_native_over_real_jdk_bytecode`
forces it ahead of the real bytecode at every dispatch site.

That is the difference between the two rows this lane owns, and it is worth
stating plainly: **the `ForkJoinPool` defect is a live wrong ANSWER on the
default configuration, and the `active` half of it is dead code there.**

## 3. What the defects actually are

### 3.1 `Continuation` — a guard that could not fire, and three type-confused slots

`run()` read `ctx.get_field(this, NEW15_CONT_STATE)` and matched `Value::Int`.
Slot 2 of a real `Continuation` is `parent`, a reference, so the match fell
through to `_ => NEW15_CONT_STATE_NEW` and `prev_state == DONE` was unreachable
regardless of how many times the continuation had completed. HotSpot refuses the
second `run()` with `IllegalStateException` — **measured**, §5.

The `<init>` native wrote five slots, 0..4. On a real receiver that is
`target scope parent child tail`: the scope went into `target`, the target into
`scope`, and `Int(0)` was stamped into `parent`, `child` and `tail` — three
reference fields that real JDK bytecode reads (`getParent()`, `innermost()`,
`isEmpty()`). §4 is what happens to those three writes.

`pin()`/`unpin()` maintained a counter in slot 3 (`child`). Nothing outside that
pair ever read it; the observable effect of both is `ctx.vt_pin()`/`vt_unpin()`,
which is unchanged.

### 3.2 `ForkJoinPool` — a wrong answer on a pool the native did not allocate

`commonPool()` allocates its proxy through `try_alloc_concurrent_synthetic`,
which clamps the request up to the loaded class's width, so on the real-JDK path
the object is a genuine 16-slot `ForkJoinPool`. It then wrote
`Value::Int(NEW15_COMMON_POOL_PARALLELISM)` into slot 0 (`termination`) and
`Value::Int(0)` into slot 1 (`saturate`). `getParallelism()` read slot 0 back.
Native writes, native reads, they agree — the exact write-then-read-the-same-
wrong-slot shape that makes this species invisible to a round-trip test.

It is not invisible on a pool the native did **not** allocate.
`new ForkJoinPool(4)` is built by real bytecode, which sets the real
`parallelism` at index 15; `getParallelism()` is forced to the native (§2), reads
slot 0 — a null `CountDownLatch` — misses the `Value::Int` arm and returns its
fallback `1`. HotSpot returns 4, and so does the pool's own `toString()` under
CratonVM, because `toString()` is real bytecode nobody registered over. That
disagreement between two reads of the same object is the finding, and it is what
the probe prints.

## 4. Is the `Int`-in-a-reference-slot collector-visible? No — and the three
impls of that primitive disagree about why

W7-69 §6(2) files this as "the GC-visible half … an `Int` in a slot the collector
scans as an oop". **That is not what happens, in any of the three field-store
implementations in `gc/`,** and the correction matters because it changes what a
follow-up lane should look for.

| store | what an `Int` written to a declared-**reference** slot becomes | collector sees |
|---|---|---|
| `gen_heap.rs:3935` compact arm | **autoboxed** into a 1-field `AUTOBOX_CLASS_ID` wrapper; `get_field` (`gen_heap.rs:3634`) unboxes it back | a valid object |
| any legacy 16-byte `Value` cell | the raw `Value::Int`, stored as-is | nothing — `for_each_ref_slot`'s legacy arm (`gen_heap.rs:16183`) matches `Value::Object(Some(_))`, so a non-reference tag is simply not visited |
| `zgc.rs:5243` and `g1.rs:9050` compact arms | `cratonvm_types::write_compact_field`'s `FieldStorageKind::Reference` arm maps every non-`Object` value to **raw 0** (`types/src/field_layout.rs:1203-1207`) — the write is silently **dropped to null** | a null slot |

So there is no bogus pointer to mark or move, on any collector. What there is
instead is a **three-way disagreement about one primitive**: `gen_heap` preserves
the value through a wrapper, ZGC and G1 discard it. ZGC has been the default
since 2026-08-10 and `compact_ref_fields_enabled()` defaults to `true`
(`types/src/field_layout.rs:174-185`), so the default configuration is the
discarding one.

That reconciles the two halves of the defect rather than leaving them as separate
claims: **the completed-continuation guard could not fire on either mechanism.**
Under ZGC/G1 slot 2 reads back `Object(None)`; under `gen_heap` it reads back
`Object(Some(wrapper))`. Both miss the `Value::Int` arm. W7-69's conclusion is
right; its stated cause is not.

Not fixed here, and deliberately: a native that type-puns a primitive into a
declared-reference slot is a population, not a site — W7-69 §4 lists a dozen —
and "which of the three stores is correct" is a collector-design question that
needs a build and an owner. It is filed in §8.

## 5. Proving the RED

### 5.1 The HotSpot oracle, measured

`probes/ContinuationForkJoinPoolAliasProbe.java`, run on Adoptium 25.0.3.9
(transcript preserved verbatim in
`probes/ContinuationForkJoinPoolAliasProbe.expected.txt`):

```
CONT scope-via-toString-holds-name=true
CONT scope-via-toString-holds-lambda=false
CONT body-ran-count=1
CONT second-run=THREW:java.lang.IllegalStateException
CONT body-ran-count-after-second=1
FJP explicit-pool getParallelism=4 toString-parallelism=4 agree=true
FJP common-pool getParallelism=31 toString-parallelism=31 agree=true
FJP common-pool getCommonPoolParallelism=31
```

**Every read in that probe is a real JDK accessor, never the native that wrote
the field.** `Continuation.toString()` is JDK bytecode appending the real `scope`
field; `ForkJoinPool.toString()` is JDK bytecode rendering the real
`parallelism`. Neither is registered as a native here, so neither can agree with
the thing under test by construction. That is the whole difference between this
probe and one that passes while the object is wrong.

**The guard's observable is the refusal, not the run.** `CONT body-ran-count`
and `CONT body-ran-count-after-second` are printed side by side precisely so
"the continuation ran" cannot be mistaken for "the guard works": before the fix
the second is 2, after it is 1.

The one row that legitimately differs on CratonVM is the common pool's absolute
number — the proxy is clamped to 1 for determinism (`NEW15_COMMON_POOL_PARALLELISM`,
T16.7) — so the comparable assertion there is `agree=true`, accessor and
`toString()` reporting the same number, not the value. That is written down in
the `.expected.txt` rather than left for a reader to rediscover.

### 5.2 The unit half

Nine tests in `new15_tests`. The load-bearing one is
`the_completed_guard_fires_on_the_real_layout_and_the_old_predicate_did_not`,
which reproduces the **old** slot-2 predicate verbatim beside the new one, on the
same real-layout receiver, and asserts the old one does **not** fire. A test that
exercised only the new path would pass before and after the change, which is the
vacuous shape this campaign keeps paying for.

The fallback tests declare the fabricated `_f0..` shape
(`ClassManager::instance_fields`) so the class-side witness is **falsifiable**
under the mock rather than constant — the trap `native-api/src/test_mock.rs`'s
own `resolve_field_index_by_class_id` doc comment records. `native-builtins`'
mock has a `mock_field_slot` fallback table that answers for many JDK field
names; it was checked and names **none** of
`scope target done preempted parent parallelism termination active`, so a
declared-fields miss under this mock really is a miss.

## 6. The repair

Name-first with a slot fallback — the standing W4-4 remedy, and the form
`native-io`'s repaired `bb_resolve_heap_array` already uses. The maps are
**published, not renumbered**: renumbering fixes one reader and can break another
that agreed with the old numbering, and a synthetic receiver still needs the old
indices.

* `ContSlots` / `cont_slots(ctx, this)` resolve `scope`, `target`, `done`,
  `preempted` on the receiver's own `ClassId`. The witness is **all four**
  resolving, not any one: a partially-named layout that mixed real and synthetic
  indices inside one object would be worse than either.
* The four-value `state` int has no real counterpart, so on a real receiver it
  maps onto `done`, the `boolean` the JDK's own `isDone()` returns.
  `cont_is_done`/`cont_set_done` carry the encoding; `NEW15_CONT_STATE_YIELDED`
  was already `#[allow(dead_code)]` and nothing ever read `RUNNING`, so the whole
  machine really is one flag on a real class. On a synthetic receiver the
  four-value encoding is preserved byte-for-byte.
* `ContSlots::pin` is `Option<usize>` and is `None` on the real class, which
  declares no pin counter — `Continuation.pin()` is `static native` there and the
  count is VM state. The bookkeeping is skipped rather than stamped over `child`.
  `vt_pin`/`vt_unpin`, the only observable half, are untouched.
* `FjpSlots` / `fjp_slots(ctx, pool)` resolve `parallelism` — the one name the
  synthetic map and the real class agree on the MEANING of while disagreeing on
  the index — and `active`, which is `None` on the real class. `commonPool()`
  therefore writes its parallelism into the real `int` field and skips the write
  that had nowhere to go.
* `NEW15_CONT_SLOT_MAP` and `NEW15_FJP_SLOT_MAP` are `read_alias::SlotMap`
  values, published unconditionally from their registrars. Unconditional and
  outside the flag check for the reason `native-io`'s `BB_SLOT_MAP` is: gating
  publication would leave a run that enables the flag later with nothing to
  sweep.

**Which mode does this touch, and why is it justified?** Both, and Compatible
mode (`--real-jdk`) is where the defect lives — the synthetic arm was already
correct because there the fabricated layout IS the map. Compatible mode is
contractually frozen **except genuine HotSpot-parity bug fixes**, and both
changes are exactly that, each with a measured HotSpot answer beside it: a second
`Continuation.run()` throws `IllegalStateException` (HotSpot: throws; before:
ran the body again), and `getParallelism()` on an explicitly-sized pool returns
that size (HotSpot: 4; before: 1). Nothing else in either mode changes: the
synthetic fallback is the previous behaviour verbatim, asserted by
`cont_slots_falls_back_to_the_synthetic_map_on_an_anonymous_class` and
`the_completed_guard_keeps_the_state_int_encoding_on_a_synthetic_receiver`.

**No `CRATONVM_*` flag was added.** `CRATONVM_DBG_LAYOUT_ALIAS` is reused through
`layout_alias::enabled()`, as W7-69 established; a new name would need four files
or `cargo test -p cratonvm-types` goes red.

### 6.1 The observation is gated so it can stay quiet

Both `observe_read` blocks sit on the *fallback* branch, and inside that branch
they are further conditioned on the receiver declaring a real-JDK-only name
(`parent` for `Continuation`, `termination` for `ForkJoinPool`). Without that,
every synthetic run would print five `scope → _f0`-shaped rows: true statements,
and pure noise, because on a fabricated class the native's slot map is the
class's truth. The state worth printing is the one that cannot happen by
construction — a receiver carrying SOME real field name that still failed the
witness. W7-69 §5.1's own lesson, applied in the other direction: an instrument
that fires on every read is a probe that cannot fail.

The gate is spelled `if layout_alias::enabled() {` **exactly**, and the modules
are imported rather than named by full path, because
`every_read_side_observation_is_gated_and_observation_only` scans for that
literal — a fully-qualified `if cratonvm_native_api::layout_alias::enabled() {`
reads as *ungated* to the instrument's own gate. That was caught by simulating
the gate, not by reading it.

## 7. Gate simulation

This lane cannot run `cargo`. The three gates of
`native-api/tests/read_alias_coverage.rs` that this change is exposed to were
re-implemented outside the tree — including their `strip_comments` and
`match_brace` helpers — and run against the tree and against mutated copies.

| gate | on this tree | on its mutation |
|---|---|---|
| `there_is_exactly_one_read_side_detector` | green (`native-api/src/read_alias.rs` only) | not mutated — this change adds no emitter |
| `every_read_side_observation_is_gated_and_observation_only` | green | **red** when one gate is re-spelled `if cratonvm_native_api::layout_alias::enabled() {` |
| `every_declared_slot_map_is_published` | green (4 maps, 0 orphans) | **red** when `declare_slot_map(&NEW15_FJP_SLOT_MAP)` is unwired |

Green-on-the-tree *and* red-on-the-mutation, both, for the same reason W7-69 §5.1
gives: a gate never seen to fail is the most common wasted effort here, and one
of its six was RED on an untouched tree the first time anybody checked.

## 8. What this lane did not fix, and why

1. **The three-way store disagreement of §4.** `gen_heap` autoboxes a primitive
   written to a declared-reference slot; ZGC and G1 null it. That is one
   primitive with three implementations, which is the shape this project has
   already paid for once ("one sick collector ⇒ diff the two impls of the same
   primitive"). It is not a `Continuation`/`ForkJoinPool` question — it affects
   every native that type-puns, and W7-69 §4 lists a dozen. Deciding which of the
   three is correct needs a build and an owner.
2. **`Continuation.pin()`/`unpin()` are `static native` in the real JDK, and both
   registrations begin `obj_arg(args, 0)?`.** A static native is dispatched with
   no receiver, so `args` is empty and `obj_arg` returns `Err`. Source-level only
   — nothing was run — and it is a different species (an arity/receiver
   assumption, not a slot alias), so it is recorded rather than folded in. A lane
   picking it up should measure what CratonVM actually passes for a zero-arg
   static native before changing anything.
3. **`java/util/concurrent/ForkJoinPool.getActiveThreadCount()I` is dead on the
   default path** (§2) — its registration is dropped by the real-ForkJoinPool
   filter. It is repaired anyway, because it is live under
   `CRATONVM_SYNTHETIC_FORKJOINPOOL`, but no probe covers it and none should
   pretend to.
4. **The published maps still disagree with the real classes, and that is the
   point.** `verify_declared_slot_maps` will still report seven wrong-field rows
   for these two classes, exactly as it does for `native-io`'s `BB_SLOT_MAP`.
   Since 2026-08-12 that sweep actually has a caller
   (W7-90-slot-map-sweep-caller.md); the seven rows are §4.2 and §4.3 of that
   record, still predicted rather than transcribed.
   Those rows are the fallback's honest description; the reason they no longer
   describe a defect is that no real receiver reaches the fallback. The row that
   leaves the census is not one of these — it is the runtime `observe_read`
   census, which after this change has nothing to print for either class on a
   real receiver.
5. **Runtime confirmation of anything.** The probe's CratonVM column is unrun.
   `probes/ContinuationForkJoinPoolAliasProbe.expected.txt` states what each line
   must read before and after, so the first run is a diff rather than an
   interpretation.
6. **The other rows of W7-69 §6 are untouched**: `java/nio/ByteBuffer` slots 6
   and 4, `StringJoiner`'s swapped `prefix`/`delimiter`, `Method`'s 11-of-12
   legacy map, and the four synthetic-only maps. Each is a different registrar
   and a different guard question.
