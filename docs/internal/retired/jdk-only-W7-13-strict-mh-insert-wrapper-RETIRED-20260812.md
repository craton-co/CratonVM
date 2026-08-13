# `__mh_insert_wrapper__` — ten combinator carriers went through the wrong door

> # RETIRED-FIXED 2026-08-12 — moved out of docs/known-issues/jdk-only/
>
> **Evidence: this record's own falsifier, run on the rebuilt binary.**
> RETIREMENT-20260812B.md §1.2. `MhFamilyProbe` re-created from the table below
> and run three ways — HotSpot 25, `cratonvm-f8 --jdk-only`, pristine-dev
> control — gives **12 of 12 identical to HotSpot** where this record measured
> ten `NoClassDefFoundError`s, and `--jdk-only-report` over that run holds
> **zero** rows whose `class` begins `__mh_`. A green `RJdkHandles` was
> deliberately NOT accepted as the evidence, for the reason *How to falsify
> this* gives.
>
> **Both "observed on the way" rows are closed too, measured not assumed:**
> `asCollector` under `--real-jdk` answers HotSpot's `6`, and `bindTo` on a
> leading `int` refuses with HotSpot's exact `IllegalArgumentException: no
> leading reference parameter` on both arms.
>
> **The one open row was transplanted, not dropped.** The second, disagreeing
> `java/lang/invoke/MethodHandle` slot map in `classloader.rs` is now stated
> inline in W7-19-methodhandles-compatible-residuals.md §5.2.1, which is live.

**Status: DIAGNOSED and FIXED IN SOURCE 2026-08-11. NOT REBUILT.** Every
measurement below was taken by running the already-built `dev` binary at
`C:/craton/CratonVM/target/release/cratonvm.exe`; nothing in this record claims
that the source change works, only that the measurements were taken.

> **RETIRE-FIXED CANDIDATE as of 2026-08-12.** Both of this record's two
> remaining items are closed: the stale §9 row in
> `docs/architecture/natives-over-real-jdk-classes.md` is edited in place, and
> the neighbouring `lk_previous_lookup_class` slot-2 claim has been checked and
> is **also stale** — the function resolves the slot from a class-side witness
> and coerces a non-reference read to null, so it can neither read slot 2 on the
> real layout nor return an `Int` from a `()Ljava/lang/Class;` native. See *Both
> §9 items are now settled*, below. Nothing is left to apply. What the record
> still needs before it is retired is a REBUILD of the headline carrier fix,
> which is the `--jdk-only-report` command in *How to falsify this*. One new
> subject was found on the way and is NOT closed: a second, disagreeing slot map
> for `java/lang/invoke/MethodHandle` in `classloader.rs`, almost certainly
> unreached, recorded in that same section.

Branch: `fix/jdk-only-strict-mh-insert-wrapper-20260811`.
File changed: `native-builtins/src/lang_invoke.rs`, and nothing else.

> **Tag collision, so nobody misreads it.** `native-builtins/src/classloader.rs`
> already carries a comment tagged `W7-13` about a second `defineClass2`
> ByteBuffer decoder. That tag was never a record in this directory; this file
> is a different subject that was assigned the same number. Neither refers to
> the other.

---

## The failure, and the assertion it reaches

`W7-11-strict-baseline-remeasured.md` lists `RJdkHandles` among six
pre-existing strict failures and gives its identity as
`NoClassDefFoundError: __mh_insert_wrapper__`. Reproduced today:

```sh
./target/release/cratonvm.exe --jdk-only \
  --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles
```

```
CK RJdkHandles invoke ok type=(int,int)int
Exception in thread "main" java/lang/NoClassDefFoundError: __mh_insert_wrapper__
	at RJdkHandles.adaptation(RJdkHandles.java:110)
```

`lookupAndInvoke()` completes. The vector dies on the **first** assertion of
`adaptation()`, `RJdkHandles.java:110`:

```java
MethodHandle plus10 = MethodHandles.insertArguments(add, 0, 10);
check((int) plus10.invokeExact(5) == 15, "insertArguments");     // <- line 110
```

The same command with `--real-jdk` passes the whole vector.

## `__mh_insert_wrapper__` is not a JDK class

Two independent checks against the JDK 25 image on this host
(`Eclipse Adoptium jdk-25.0.3.9-hotspot`):

```text
javap -p __mh_insert_wrapper__                     -> class not found
javap -p java.lang.invoke.__mh_insert_wrapper__    -> class not found
jimage list lib/modules                            -> 28,256 entries,
                                                      0 matching "mh_insert",
                                                      0 classes whose simple
                                                      name begins with "__"
```

It is a name CratonVM mints. `grep` finds it, and its nine siblings, at eleven
allocation sites, all of them in `native-builtins/src/lang_invoke.rs` and
nowhere else in the workspace.

## What the carriers actually are

Not a model of `LambdaForm` and not a stand-in for a `BoundMethodHandle`
species. Each is a **2- or 3-slot tuple** holding the state one combinator
captured, so the matching arm of `mh_dispatch` can apply the combinator at
invoke time. `MH_KIND_INSERT`, in full:

```rust
let target = ctx.get_field(wrapper, 0);   // the target MethodHandle
let values = ctx.get_field(wrapper, 1);   // the pre-bound Object[]
let pos    = ctx.get_field(wrapper, 2);   // Int — where to splice them
// ...splice `values` into `extra_args` at `pos`, then dispatch `target`.
```

The three fields are written by the `insertArguments` native and read by that
arm. Nothing else in the workspace touches the class: no native is registered
on it, no bytecode names it, no `checkcast` or `instanceof` reaches it, and it
has no `synthetic_stub_fields` arm (so it declares zero fields and the objects
carry the three slots the allocation asked for — which is what the reads use,
and which this change does not alter).

**So the combinator is already applied at dispatch.** That matters for the
decision below.

## The diagnosis: the wrong mint door, and the census says so itself

All eleven sites called `try_alloc_concurrent_synthetic`, which is the
**compatibility stand-in** door: it reaches
`ClassManager::try_ensure_synthetic_class`, which stamps
`ClassOrigin::CompatibilityStub` — the one thing `--jdk-only` forbids
(contract §5). `--jdk-only-report` prints the misclassification in its own
words:

```json
{"kind":"compatibility-class-requested",
 "class":"__mh_insert_wrapper__",
 "requester":"native-builtins\\src\\lang_invoke.rs:6219",
 "reason":"VM-requested stand-in: ensure_synthetic_class called with no class
           file on any classpath entry"}
```

The `reason` is half right and half wrong in a way that is the whole defect:
there is indeed no class file on any classpath entry, and nothing is being
**stood in for**. A stand-in is a fabrication that pretends to be somebody's
real class. This is a tuple CratonVM invented to hold its own state.

Two cautions from earlier in this campaign, both checked rather than assumed:

* **A slash-form `NoClassDefFoundError` is not a `<clinit>` verdict.** This one
  is not slash-form and is not a `<clinit>` verdict either — the class is
  reached from a native's allocation, never from constant-pool resolution, and
  the refusal is raised at the fabrication point by policy.
* **"The natives bound to it are unreachable" is a claim about registration.**
  For these names it is true and irrelevant: there are no natives bound to them
  and none are wanted. The census in the same run shows the combinator natives
  themselves are registered and *did* run —
  `insertArguments`, `permuteArguments`, `filterArguments`, `guardWithTest`,
  `foldArguments`, `collectArguments`, `catchException`, `filterReturnValue`,
  `asCollector`, `asSpreader` all appear as
  `native-shadows-bytecode / bridge-ran-over-bytecode`. The natives ran; what
  failed was the allocation inside them.

## The `NoClassDefFoundError` names one carrier. Ten are refused.

`RJdkHandles` dies on the first one it reaches, which makes the vector a
one-bit instrument. A probe that reaches each combinator independently and
catches per step (`MhFamilyProbe`, written for this record, not checked in)
resolves it in a single run:

| step | HotSpot 25 | CratonVM `--jdk-only` | CratonVM `--real-jdk` |
|---|---|---|---|
| `insertArguments`   | OK | `NoClassDefFoundError: __mh_insert_wrapper__` | OK |
| `permuteArguments`  | OK | `__mh_permute_wrapper__` | OK |
| `filterArguments`   | OK | `__mh_filter_wrapper__` | OK |
| `guardWithTest`     | OK | `__mh_guard_wrapper__` | OK |
| `asCollector`       | OK | `__mh_collect_wrapper__` | **wrong value** (below) |
| `asSpreader`        | OK | `__mh_spread_wrapper__` | OK |
| `filterReturnValue` | OK | `__mh_retfilter_wrapper__` | OK |
| `foldArguments`     | OK | `__mh_fold_wrapper__` | OK |
| `collectArguments`  | OK | `__mh_collect_args_wrapper__` | OK |
| `catchException`    | OK | `__mh_catch_wrapper__` | OK |
| `dropArguments`     | OK | **OK** | OK |
| `asVarargsCollector`| OK | **OK** | OK |

The two that pass under strict are the tell. `dropArguments` and
`asVarargsCollector` carry their whole state in a **single reference**, which
goes straight into `MH_BOUND` with no carrier at all. Every combinator that
needs to carry two or three values — one of them an `int` — mints a carrier and
is refused. The split is drawn by arity, not by semantics.

The strict census for that one probe run holds exactly ten
`compatibility-class-requested` rows for the family, one per name, each
`requester` naming its own `lang_invoke.rs` line (which is what
`#[track_caller]` on the funnel buys).

## The decision

### Chosen: mint them through the VM-internal door

`ensure_vm_internal_class` (`ClassManager::ensure_generated_class` with
`ClassOrigin::VmInternal`) is the other half of the §5 API boundary: it mints
the classes a conforming JVM creates **without any class file**, contract §1
item 6 permits those in every mode, and it therefore never refuses and records
no violation. `docs/architecture/natives-over-real-jdk-classes.md` states the
test to apply: *the decision is whether the JVM specification says a class file
must exist for this name.* For a tuple CratonVM invented, it does not.

This is not a novel reading. `vm_exec::heap_alloc_object` already takes that
door for `cratonvm/synthetic/AnonymousObject$N`, with a comment that describes
this exact situation:

> *"`ensure_generated_class`, not `ensure_synthetic_class`: this is a VM
> bookkeeping type, not a compatibility substitution. There is no
> `cratonvm/synthetic/AnonymousObject$N` class file anywhere and there never
> will be … Stamping it `CompatibilityStub` made contract §11's zero-stub
> acceptance criterion unachievable by construction."*

**The door is open in strict mode as a matter of observation, not of doctrine.**
A `--jdk-only` run of the shipped binary under `CRATONVM_DBG_ANONALLOC=1` mints
16 `AnonymousObject$N`; the census for that run records a violation for none of
them and nothing is refused. That is the same door, in the same mode, in the
same process.

### Rejected: remove the need for the minted class

The instruction preferred this, and it is the right preference in general —
removing a split beats admitting one. It is not reachable here, for a reason
the measurement above supplies rather than an argument:

1. **The combinator is already applied at dispatch.** `mh_dispatch`'s
   `MH_KIND_INSERT` arm splices the bound values into `extra_args` at invoke
   time; the carrier is not a semantic stand-in for `LambdaForm`, it is where
   the three values live between the combinator native and that arm. "Apply it
   at dispatch" is the state of the tree, not a change to it.
2. **What is left is arity, and the tree has already taken the free half.**
   `dropArguments` and `asVarargsCollector` fit in one reference and use
   `MH_BOUND` directly — and both are green under strict today. The remaining
   ten need two or three values including an `int`.
3. **The only carrier-free container for them is a reference array, and an
   `int` in a reference array element is the §5 heap-corruption shape** — the
   collector scans the element as an oop, which is precisely the defect
   `W4-4-slot-index-species-sweep.md`,
   `fixed-bugs/jdk-only-W6-3-slot-index-species-residuals-FIXED-20260811.md`
   and `docs/architecture/natives-over-real-jdk-classes.md` §5 are about. Boxing
   `pos`/`count` into a real `java/lang/Integer` avoids that, but it rewrites
   the allocation shape of ten combinators across 21 mint-and-read sites, in
   `Compatible` mode as well as strict, with no build available to check any of
   it. `Compatible` must stay byte-for-byte, so that is disqualifying on its
   own.
4. The other carrier-free option — moving the values into spare
   `java/lang/invoke/MethodHandle` slots — means widening an object that is
   already over-allocated at 21 slots against a real class that declares 6, ten
   kinds over. That trades a policy misclassification for a layout hazard.

A future lane that wants (1) should start at the arity, not at the door: a
carrier whose state is one reference does not need a class, and that is already
true of two of the twelve.

## The change

One helper, eleven call sites, one file.

```rust
#[track_caller]
fn alloc_mh_carrier(ctx: &mut dyn NativeContext, name: &str, num_fields: usize) -> ObjectRef {
    let cid = ctx.ensure_vm_internal_class(name, num_fields);
    let n = num_fields.max(ctx.class_num_total_fields(cid));
    ctx.try_alloc_object_gc_safe(cid, n)
        .unwrap_or_else(|| ctx.alloc_object(cid, n))
}
```

The width clamp and the GC-safe-then-aborting allocation are
`try_alloc_concurrent_synthetic`'s, kept verbatim so the allocation itself is
unchanged. The helper is infallible because the door is, so the eleven sites
drop a `?` for a refusal that was never theirs to propagate.

Checked against the three fabrication traps, since the change mints:

* **`$` in the name → abstract interface unless carved out.** No carrier name
  contains `$`. Unchanged from before, and the names are not touched.
* **No `synthetic_stub_fields` arm → every `set_field_by_name` discarded.** The
  carriers have no arm and never used `set_field_by_name`; all reads and writes
  are positional `get_field`/`set_field` against the slot count the allocation
  requested. Also unchanged.
* **`new_object_initialized` stores nothing.** Not used here; the carriers are
  allocated and written field-by-field by the native.

`t9c_synthetic_field_tables_cover_their_factories` scans literal
`alloc_concurrent_synthetic(ctx, "name", n)` sites and skips any class whose
table declares 0 fields. The carriers declare 0, so they were already skipped;
after the change they are not scanned at all. `stub_ratchet` counts
`SyntheticStub`-tagged **registrations**, which this change does not touch.

## Which mode each change affects

| | effect |
|---|---|
| `JdkOnly` (`--jdk-only`) | the refusal goes; ten `compatibility-class-requested` violations should leave the census |
| `Compatible` (`--real-jdk`) | **unchanged** — both doors fabricate in this mode |

Two second-order differences exist in `Compatible`, and both run the safe way:
`fabricate_class` no longer runs its full-classpath rescan per carrier looking
for real bytes that cannot exist (memoised after the first, but the first ran),
and the carriers stop being counted against a zero-stub census they were never
evidence for.

## How to falsify this

Rebuild and re-run, in this order — the first two are the claim, the third is
the guard:

```sh
# 1. the vector, strict. Was: NoClassDefFoundError at adaptation:110.
cratonvm --jdk-only --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles

# 2. the family, strict. Was: ten NoClassDefFoundErrors, two OKs.
#    Re-create MhFamilyProbe from the table above; catch per step, do not let
#    the first failure end the run — that is the mistake RJdkHandles makes.

# 3. Compatible must not move.
cratonvm --real-jdk --java-home "<jdk-25-home>" -cp regression-suite/build RJdkHandles
```

`--jdk-only-report <FILE>` on run 1 is the sharper instrument: the ten
`compatibility-class-requested` rows whose `class` begins `__mh_` should be
gone, and no row should have appeared in their place. **A green `RJdkHandles`
alone is not enough evidence**: it stops at the first combinator, so it cannot
distinguish "ten carriers admitted" from "one admitted and the vector got
further before failing on something else".

## Two things observed on the way, not fixed here

Both are `Compatible`-mode behaviour, out of this record's scope, and neither is
touched by the change.

1. **`asCollector` returns a wrong value under `--real-jdk`.** With
   `sumAll(int[])`, HotSpot 25 gives `sumAll.asCollector(int[].class, 3)
   .invoke(1,2,3) == 6`; CratonVM `--real-jdk` disagrees. The
   `MH_KIND_COLLECT` arm gathers into a **reference** array and boxes each
   element, unconditionally — see its own comment, which explains the boxing as
   a fix for Groovy's `Object[]` call sites. For a collector whose array type
   is `int[]` that is the wrong container. This is the shape
   `fixed-bugs/jdk-only-W2-5-methodhandles-arrayelement-combinators-FIXED-20260811.md`
   records — a combinator that answers plausibly without doing the right thing
   — and it wants its own vector: `RJdkHandles` only ever calls `asCollector`
   through
   `asVarargsCollector`, which takes a different path and passes.
2. **`bindTo` on a leading `int` parameter.** HotSpot refuses
   `findStatic(...(String,int)int).bindTo("abc").bindTo(2)` with
   `IllegalArgumentException: no leading reference parameter`; CratonVM
   `--real-jdk` accepts it and answers 5. A missing negative check, not a wrong
   value — the same family as
   `fixed-bugs/jdk-only-W3-1-invokeexact-must-not-fabricate-a-zero-FIXED-20260811.md`.

## Both §9 items are now settled (2026-08-12)

> **This record's two remaining items are CLOSED.** The stale §9 row is edited in
> place in `docs/architecture/natives-over-real-jdk-classes.md`, and the
> neighbouring claim it flagged as unchecked has now been checked — and is stale
> too. Nothing is left to apply from this record. What follows is the original
> §9 write-up, kept because the second half is now a measurement rather than a
> deferral.
>
> **The neighbour: `classloader.rs::lk_previous_lookup_class` reading slot 2
> unconditionally. CHECKED. The claim is FALSE of the tree.** The function reads:
>
> ```rust
> let value = match lk_real_prev_lookup_class_slot(ctx, this) {
>     Some(slot) => ctx.get_field(this, slot),
>     None => ctx.get_field(this, LK_PREVIOUS_LOOKUP_CLASS),
> };
> Ok(Some(match value {
>     Value::Object(_) => value,
>     _ => Value::Object(None),
> }))
> ```
>
> Three separate things are right about it, and the audit row predates all three:
>
> 1. **The slot is chosen by a CLASS-side witness**, `lk_real_prev_lookup_class_slot`
>    → `resolve_field_index_by_class_id(class_id_of_object(obj), "prevLookupClass")`.
>    That is the same witness `lk_real_allowed_modes_slot` uses, and the module's
>    doc comment says explicitly that `Some` from one and `Some` from the other are
>    *the same layout verdict* — which is what lets `alloc_lookup`, `lk_set_modes`,
>    `lk_modes_of` and this function agree about which object they are holding.
> 2. **`LK_PREVIOUS_LOOKUP_CLASS` (= 2) is reached only on the FABRICATED
>    layout**, where the witness has already answered `None`. On the real JDK 25
>    layout (`lookupClass`(0), `prevLookupClass`(1), `allowedModes`(2),
>    `cachedProtectionDomain`(3)) the resolved slot is used instead, so the `Int`
>    at slot 2 is never read as a `Class`.
> 3. **The descriptor is enforced at the return.** Whatever the layout turned out
>    to be, a non-reference read is coerced to `Value::Object(None)` — and `null`
>    is this method's own legal answer, because CratonVM models no modules and
>    nothing ever populates the field. So the `()Ljava/lang/Class;` native cannot
>    hand back an `Int` on ANY layout, including one this VM does not model.
>
> **This is the "loud if it is wrong" answer the brief asked for, and it is loud
> in the good direction: slot 2 is not wrong on the real layout, because slot 2 is
> not read on the real layout.** No slot-count witness was widened to reach that
> verdict, and none needed to be: the fix already in the tree replaced the raw
> index with a resolved one rather than justifying the raw index.
>
> **Not settled, and not this item: a SECOND, disagreeing slot map for
> `java/lang/invoke/MethodHandle`.** Found while checking the above.
> `classloader.rs:9084-9090` declares `MH_BASE = 16` with
> `MH_KIND`(16), `MH_TARGET_CLASS`(17), `MH_NAME`(18), `MH_TYPE`(19),
> `MH_CLASS_ID`(20) and a comment asserting it *"matches the layout used by
> lang_invoke::alloc_method_handle (MH_BASE = 16)"*. It does not: `lang_invoke`'s
> map is `MH_CLASS`(16), `MH_NAME`(17), `MH_DESC`(18), `MH_KIND`(19),
> `MH_BOUND`(20). Only the BASE matches; the field order does not, so slot 16
> holds a `String` reference in one map and an `Int` in the other, and slot 20
> holds a reference in one and a `ClassId` `Int` in the other. Its only allocator
> is `classloader::alloc_method_handle`, whose only callers are `lk_unreflect` /
> `lk_unreflect_special` — and `unreflect` is registered TWICE on
> `MethodHandles$Lookup`, by `classloader.rs:9752` and by
> `lang_invoke.rs:10754`, so one of the two is dead by registrar order.
> W7-19 §3.3's census answers which: its `unreflect` row reports
> `type()` = `(H,int)int`, which is `lang_invoke`'s shape —
> `classloader::alloc_method_handle` passes `method_type: None` and would report
> `()void`. So the disagreeing map is very probably UNREACHED, which is why it has
> never corrupted anything, and it is exactly the shape that stops being harmless
> the moment registrar order changes. It wants a `--dump-native-registry` diff and
> then a deletion, not a repair. Not taken here: it is a third subject on a
> two-item record.

## One correction to `docs/architecture/natives-over-real-jdk-classes.md`

That document's §9 lists, among findings it could not correct itself, an
un-recorded slot-index residual **in this file**:

> *"`native-builtins/src/lang_invoke.rs::lk_write_allowed_modes` still does a
> bare `ctx.set_field(obj, 1, Value::Int(modes))` on its error branch with no
> class-side witness."*

**That row is stale.** The function was rewritten on 2026-08-07 (`dcfe77cb8`)
and its two arms are now mutually exclusive by construction:
`lk_allowed_modes_slot` is the class-side witness
(`resolve_field_index_by_class_id(..., "allowedModes")`), the re-assert on a
disagreeing read-back goes through **that resolved slot**, and the bare
`LK_SYNTHETIC_ALLOWED_MODES` (= 1) write is reachable only when the witness has
already said the receiver has no real `allowedModes` field — i.e. on the
synthetic 2-field layout, where slot 1 *is* `allowedModes`. The in-file comment
names the old defect explicitly as the thing it replaced.

The audit was source-only and its §9 is honest about that. This is the shape
the campaign already has a rule for: an audit row can be stale while its
neighbours are live, so run every row. The neighbouring claim in the same
paragraph — `classloader.rs::lk_previous_lookup_class` reading slot 2
unconditionally — was in a file that lane did not own and was **not** checked
then. **It has been checked now, and it is stale too — see the CHECKED block at
the top of this section. The §9 row itself is edited in place as of
2026-08-12.**

## Out-of-file patch (not applied)

None. The defect, all eleven mint sites and the fix are contained in
`native-builtins/src/lang_invoke.rs`.
