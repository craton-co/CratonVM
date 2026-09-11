# The BindableTests residual is a STALE RECEIVER, not a lost write: a native held the assertion across `Objects.<clinit>`

| | |
|---|---|
| **Status** | **ROOT-CAUSED AND FIXED**, 2026-09-11. |
| **Was** | `docs/known-issues/springboot/bindabletests-assertj-objects-field-null-under-gc-stress-20260909.md` — "one BindableTests method fails an AssertJ NPE at `CRATONVM_DBG_GC_STRESS <= 262144`, and every stale-reference probe is silent". Retired into this page. |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Reproduced identically with `--nojit`. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64 |
| **Defect** | `native-builtins/src/test_frameworks.rs`, `native_assertj_lightweight_comparable_assert` — the `objects` store reused a receiver handle taken before `org/assertj/core/internal/Objects`'s `<clinit>` |
| **Family** | the residual of [the ten-`ObjectRef` family](bindabletests-stale-objectref-family-across-allocation-20260909.md), and the first member of it that is RECEIVER-side |

## The one sentence

`Assertions.assertThat(Comparable)` is a CratonVM native, not bytecode; the
native builds the `GenericComparableAssert` field by field; and the store into
`objects` used an `assertion` handle that was read *before* the call that runs
`org/assertj/core/internal/Objects.<clinit>`. On the one call where that
`<clinit>` was still pending, its allocations moved the assertion three times,
so the store landed in a copy nothing would ever read again and the surviving
object kept `objects == null`.

## Why every probe read zero

The predecessor page's central puzzle was that `[deadref-store]`,
`[deadref-pin]`, `[deadref-arg]`, `[deadref-local]`, `[deadref-push]`,
`[deadref-nret]`, `[deadref-capture]`, `[deadref-singleton]`, `[heap-stale]`,
the three `[tlab-audit]` tripwires and `[rset-verify]` were ALL silent on a run
that demonstrably lost a field.

There are two reasons, stacked, and the second is the one that matters.

**One: every one of those arms screens a VALUE.**
`GenerationalHeap::note_deadref_store` opens with
`if let Value::Object(Some(p)) = value` and asks `dead_young_ref_reason(p)`. The
value stored here is `Objects.INSTANCE` — an old-gen singleton at
`0x20042718d88`, live from before the first collection to after the last. It is
the OBJECT BEING STORED INTO that was dead, and nothing in the family asked
about that operand. `[deadref-recv]`, added with this fix, is that missing arm.

**Two: this particular store never reached the heap at all.** The native writes
through `NativeContext::set_field_by_name`, which resolves the field against the
class of whatever object is AT the address it is handed — and **drops the store
silently when it does not resolve**:

```rust
if let Some(index) = resolve_field_index_in_hierarchy(class_id, field_name, &cm.class_store) {
    self.shared.mem.heap.set_field(obj, index, value);
}   // ← and nothing at all otherwise
```

The stale address did not validate as an object start, so `class_id_of` answered
`ClassId(0)`, `java/lang/Object` does not declare `objects`, and the write
evaporated one level ABOVE `gen_heap::set_field`. No `[deadref-store]`, no
`[deadref-recv]`, no `[CELLWATCH]`, no `[OBJWATCH] store`, no
`[PUTFIELD-WATCH]` — every one of those lives at or below a `set_field` that was
never called. **That is the whole of the silence**, and no number of arms on the
heap-side family could have closed it.

`[field-by-name-dropped]` is the arm that can, and it names this defect in one
line on the unfixed binary:

```text
[field-by-name-dropped] #4 obj=0x7a242e2000f0 recv_class=java/lang/Object recv_class_id=0 field="objects"
  … note_field_by_name_dropped ← set_field_by_name
    ← native_assertj_lightweight_comparable_assert
      ← native_assertj_comparable_assert_that ← safe_native_call
```

It is deduped by `(receiver class, field name)` for a measured reason: the same
run drops **2 710** stores, ~2 700 of them one benign shape
(`StringBuilder.toStringCache`), and an occurrence-capped log spends every line
on that population without ever reaching row #4. Deduped, the whole run is nine
rows and the defect is one of them.

## The measurement

`CRATONVM_DBG_OBJ_WATCH=GenericComparableAssert` — new, see below — follows one
class's instances across evacuations instead of watching an address. The failing
object's entire life, from the `--nojit` run at 262 144:

```text
[OBJWATCH] seq=0 src=0x7067912000f0 dst=0x706771200050 size=88 promoted=false gc_age=0
           srcbody: [0]=0x0 [1]=0x0 [2]=0x0 [3]=0x20042718c98 [4]=0x7067912000f0 …
[OBJWATCH] seq=1 src=0x706771200050 dst=0x706791200060 size=88 promoted=false gc_age=1   (same body)
[OBJWATCH] seq=2 src=0x706791200060 dst=0x20042718dd8   size=88 promoted=true  gc_age=2   (same body)
```

`gc_age=0` on the first line is the load-bearing number: the object had never
been copied before, so `0x7067912000f0` is where it was ALLOCATED and where its
construction began. Slots 3 (`actual`) and 4 (`myself`) are already set there;
slots 0 (`objects`), 1 (`conditions`) and 2 (`info`) are not.

The store-side arm of the same watch prints every `set_field` whose receiver is
an instance of the class, with the Rust caller. In order, for this object:

| # | slot | field | receiver address | landed |
|---|---|---|---|---|
| 1 | 3 | `actual` | `0x7067912000f0` (young, as allocated) | yes |
| 2 | 4 | `myself` | `0x7067912000f0` | yes |
| | | *— three evacuations, ending in old gen —* | | |
| — | **0** | **`objects`** | **never seen** | **no** |
| 3 | 1 | `conditions` | `0x20042718dd8` (old gen) | yes |
| 4 | 2 | `info` | `0x20042718dd8` | yes |
| 5 | 5 | `assertionErrorCreator` | `0x20042718dd8` | yes |
| 6 | 6 | `comparatorsByPropertyOrField` | `0x20042718dd8` | yes |
| 7 | 8 | `comparables` | `0x20042718dd8` | yes |

Every store the native performs is accounted for except `objects`, which reached
`set_field` with a receiver this heap no longer recognised as that object.

And the backtrace on the first of them names the writer, which is the whole
reason the interpreter's `[PUTFIELD-WATCH]` had nothing to say:

```text
1: set_field                          gc/src/gen_heap.rs
2: native_assertj_lightweight_comparable_assert
                                      native-builtins/src/test_frameworks.rs:4746
3: native_assertj_comparable_assert_that
4: safe_native_call_impl              vm/src/vm/vm_exec.rs
5: intercept_force_registered_native  vm/src/runtime/interpreter/native_override.rs
```

**`AbstractAssert.<init>` never runs for these objects.** That is why a
`CRATONVM_DBG_FIELD_WATCH=org/assertj/core/api/` run over the whole suite logs
276 `putfield`s in `AbstractAssert.<init>` and **not one** with a
`GenericComparableAssert` or `StringAssert` receiver: both classes are built by
this native. Reading that histogram as "the constructor is invisible" rather
than as "the constructor does not run" is what cost the most time on this page;
the receiver's class in the ledger (also new) is what settled it.

## The defect, in source

```rust
let assertion_live = ctx.read_native_pin(assertion_pin, assertion);
…
ctx.set_field_by_name(assertion_live, "actual",  actual);              // lands
ctx.set_field_by_name(assertion_live, "myself",  Value::Object(Some(assertion_live)));

let objects = static_object(ctx, "org/assertj/core/internal/Objects", "INSTANCE");
//            ^ ensure_class_initialized → Objects.<clinit> → StandardComparisonStrategy,
//              PropertySupport, Failures, FieldSupport … arbitrary Java, arbitrary GC
ctx.set_field_by_name(assertion_live, "objects", objects);             // ← STALE receiver

let conditions = static_object(ctx, "org/assertj/core/internal/Conditions", "INSTANCE");
let assertion_live = ctx.read_native_pin(assertion_pin, assertion);    // ← the refresh,
ctx.set_field_by_name(assertion_live, "conditions", conditions);       //   one store too late
```

The handle is refreshed before `conditions` and before every store after it.
The `objects` store is the single site in the function that reuses a handle
across an `ensure_class_initialized`. That is exactly why `conditions`, `info`
and the rest of the object are intact while `objects` alone is null, and it is
why the failure needs a threshold low enough to put a collection inside that one
`<clinit>` — at 393 216 and above nothing moves there and all 27 tests pass.

Only the FIRST comparable assertion in a process can hit it: after that,
`Objects.<clinit>` has run and `ensure_class_initialized` is a lookup. One
process, one exposed call, one failing test.

## The fix

Every store into the assertion now goes through one closure that re-reads the
pin:

```rust
let set_on_assertion = |ctx: &mut dyn NativeContext, field: &str, value: Value| {
    let live = ctx.read_native_pin(assertion_pin, assertion);
    ctx.set_field_by_name(live, field, value);
};
```

Refreshing per STORE rather than per paragraph is the point. The previous code
was not careless — it refreshed at every store but one — it was wrong because
the correctness of each store depended on remembering what had run since the
last refresh, and one of them got that wrong. With the closure, no reader has to
remember anything, and a store added later cannot reintroduce the defect.

`myself` keeps an explicit read/store pair, because its value IS the receiver's
address and the two have to be taken at the same instant.

### Two siblings fixed with it

Found by reading the same file for the same shape — an `ObjectRef` used after a
call that can allocate, without a pin or a refresh:

1. **`native_surefire_system_property_manager_load_properties`** pinned
   `wrapper` one statement AFTER the `properties` store, so the store crossed a
   `HashMap` allocation and `native_map_init` on an unpinned receiver. The
   `placeholder` it stores was unrooted across `native_map_init` too. Both are
   now pinned across the span they are used over.
2. **`native_forkedbooter_create_surefire_properties_if_file_exists`** held
   the freshly-built `byte[]` across `try_alloc_concurrent_synthetic`, which
   allocates and can initialise a class. This one is VALUE-side, so
   `[deadref-store]` could have caught it; it never fired because the workloads
   that reach it do not run under GC stress.
3. **`ConfigurationProvider.representation()`'s receiver** was read back through
   after `invoke_virtual` had run Java (`unwrap_or_else(|| ctx.get_field_by_name(provider, …))`).
   Pinned.
4. **`Comparables.comparisonStrategy`** was read from its static and then held
   across `Failures`' `<clinit>` before being stored. Each static is now read
   immediately before the store it feeds.

## The instruments, and the gaps they closed

Four instrument changes, each one a thing that answered "nothing here" on a run
where something was in fact wrong.

| instrument | what it could not see before | now |
|---|---|---|
| `CRATONVM_DBG_DEADREF_STORE` | only the VALUE of a store was screened | `[deadref-recv]` screens the RECEIVER, same predicate, same backtrace; pinned by `a_store_through_a_vacated_receiver_is_reported`, because an instrument whose only evidence is a zero is indistinguishable from one that is not wired |
| *(new)* `[field-by-name-dropped]` | `set_field_by_name` dropped a store whose field did not resolve on the receiver's class, silently and ABOVE the heap, so no heap-side probe could see it | counted always (a non-zero total is printed at exit) and named per distinct `(class, field)` under `CRATONVM_DBG_DEADREF_STORE` |
| `CRATONVM_DBG_WATCH_CELL` | the compact reference store (`set_field`'s `storage.is_reference()` arm) was the one heap write primitive with no `cell_watch_check`; a watch aimed at a compact object's reference field answered "nobody wrote it" | reports, like every other writer |
| `CRATONVM_DBG_FIELD_WATCH` | `[GETFIELD-WATCH]`/`[PUTFIELD-WATCH]` printed the receiver's ADDRESS only, which cannot distinguish "the same object, moved" from "a different object at a recycled address" | also prints the receiver's class, `num_slots`, `gc_flags`, whether the heap still calls it an object start, and the field's byte address (compact-layout-aware, so it can be handed to `CRATONVM_DBG_WATCH_CELL`) |
| *(new)* `CRATONVM_DBG_OBJ_WATCH=<class-substring>` | nothing followed an OBJECT. An address watch cannot: under GC stress a young object is copied every cycle and a semispace is re-served from the same base, so one address names many objects | `[OBJWATCH] seq=… src=… dst=… gc_age=… srcbody:…` per evacuation, and `[OBJWATCH] store …` + backtrace per `set_field`, both keyed by the receiver's class |

`[OBJWATCH]`'s two arms are what made this page short. The copy arm dated the
loss to before the first evacuation; the store arm named the writer — by its
ABSENCE, which is what sent the hunt one level up to `set_field_by_name`.
Neither question is answerable from an address.

Honest limits, so the next reader does not over-trust them:

* `[deadref-recv]` reads **zero** on this workload, before and after the fix.
  That is not a failure of the screen — the store it would have caught never
  reached `set_field` — but it does mean this defect is not the evidence for it.
  The unit test is.
* `[field-by-name-dropped]`'s 2 710 is a population, not a defect count. Eight
  of its nine shapes on this run are natives setting a field only some
  subclasses declare, which is legitimate. The row that matters is the one
  whose receiver class is `java/lang/Object`/`ClassId(0)` — an address the heap
  refused to recognise at all.

## What this says about the family

[The ten-defect page](bindabletests-stale-objectref-family-across-allocation-20260909.md)
closed with a "standing audit": 28 candidates in `native-collections/src/lib.rs`
of the shape "an argument-derived local pinned only AFTER the function's first
allocating call", screened by `[deadref-pin]`.

That screen is necessary and it is not sufficient, and this page is the proof.
`[deadref-pin]` fires when `pin_native_root` is handed an already-dead value —
it says nothing about a value that was pinned correctly and then USED through a
handle taken before an allocation. The audit also stopped at one file;
`test_frameworks.rs` was never in it, and it contained the residual plus three
more.

The generalisation: **a pin is not a fact about a variable, it is a fact about a
read.** `pin_native_root` roots the object; only `read_native_pin` produces an
address that is valid NOW. Any `ObjectRef` that outlives a call is a stale
address in waiting, whether it is an argument, a local, a receiver or a value,
and `[deadref-recv]` plus `[deadref-store]` together are the screen that does
not care which.

## Verification

`BindableTests`, Linux x86-64, `--XX:UseGc Generational`, `-Parallel 1`.

| `CRATONVM_DBG_GC_STRESS` | before | after |
|---:|---|---|
| *(unset — the default)* | PASS | PASS |
| 524 288 | PASS | PASS |
| 393 216 | PASS | PASS |
| 262 144 | **FAIL** (`tests=27 failed=1`) | **PASS** |
| 131 072 | **FAIL** | **PASS** |
| 65 536 | **FAIL** | **PASS** |
| 262 144, `--nojit` | **FAIL** | **PASS** |

## Repro

```bash
CRATONVM_DBG_GC_STRESS=262144 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

To watch the object rather than the address:

```bash
CRATONVM_DBG_GC_STRESS=262144 \
CRATONVM_DBG_OBJ_WATCH=GenericComparableAssert \
CRATONVM_DBG_FIELD_WATCH=AbstractAssert.objects \
  … -CratonArgs @('--XX:UseGc','Generational','--nojit')
```
