# W7-84 — a primitive written into a declared-reference slot: four implementations, four answers, now one

Status: converged on **auto-boxing**, in one crate-private module (`gc/src/autobox.rs`)
that all four field-store implementations now call. The read half is put
behind a process-wide monotone latch, so `gen_heap`'s compact reference-field
read is **cheaper than it was** while `zgc`, `g1` and `heap` gain the behaviour
for one predicted branch.

> **VERIFIED AGAINST A BINARY 2026-09-02.** §5's deliverable was built and run.
> The record's own §5.4 opened "Nothing was executed. No `cargo build`, `check`
> or `test`" — that was true for **21 days**.
>
> ```text
> cargo test -p cratonvm-gc --test primitive_in_reference_slot   10 passed, 0 failed
> cargo test -p cratonvm-gc --lib autobox                         5 passed, 0 failed
> ```
>
> Every test §5.3's table names is present and green, including the two that
> exist to stop the suite going vacuous:
> `the_two_arms_of_the_ab_are_actually_different_layouts` (the A/B is not
> comparing one arm with itself) and `the_three_wrong_shapes_are_each_named`
> (`Object(None)`, a half-boxed wrapper and a raw `Int` are each rejected by
> name, so "the read returns something" cannot pass). The five in
> `autobox.rs`'s own `mod tests` are the five §5 names, `the_latch_gates_the_
> read_half_in_both_directions` among them.
>
> **The count was wrong, in the usual direction.** §5 says "seven tests"; there
> are **ten** — `there_are_exactly_four_field_store_implementations` and
> `all_four_field_store_implementations_route_through_the_shared_primitive` are
> not in §5.3's table. A shared file grew after the record was written. Verify
> the ASSERTIONS, not the count: every one of the four records repaired on
> 2026-09-01 predicted a count and every one was wrong.
>
> **§8's identity claim got independent corroboration from unrelated work.**
> §5.4 point 2 says there is no Java-level probe for this and that a Java probe
> which could not fail would be worse than none — which is right, and is why this
> was not manufactured. But the guard fires on the boot path of *any* program, so
> it turned up on its own while probing `com.sun.net.httpserver` on 2026-09-02:
>
> ```text
> WARN a non-reference value was stored into a slot the class declares as a
>   REFERENCE — boxing it into an AUTOBOX_CLASS_ID wrapper ... (W7-84)
>   class_id=ClassId(12) index=0 value=Int(-1) occurrence=0
> ```
>
> `ClassId(12)` slot `0`, reached from a probe that has nothing to do with this
> record — the same identity §8 named, from a different direction. Corroboration
> of §8, not of §§1-7.
>
> **What this does NOT verify.** §5.4 point 3 stands: the latch is still priced by
> inspection, and "not measured" is still not "free". §§1-7's source-level claims
> are unchanged — this note says the tests that were written to prove them exist,
> compile and pass, which is what the record owed.

**Nothing in §§1-7 was built or run.** This lane may not invoke `cargo`; the
orchestrator builds. Every claim about CratonVM in §§1-7 is source-level and
says so. The only thing executed for those sections was `rustfmt --check` as a
parser on the nine edited files — that proves they parse, and nothing else.

> **2026-08-12 (lane B8) — §8's identity claim is INDEPENDENTLY CONFIRMED by
> running, and the warning's own instruction does not work.**
>
> §8 named `ClassId(12)` as `java/lang/Class` and slot 0 as `cachedConstructor`.
> Confirmed on `/c/craton/jdkonly-wave2-target/release/cratonvm.exe --jdk-only`,
> on an unrelated probe class, i.e. the guard fires on the boot path of *any*
> program:
>
> ```
> WARN cratonvm::gc::guard: a non-reference value was stored into a slot the class
> declares as a REFERENCE — boxing it into an AUTOBOX_CLASS_ID wrapper …
> class_id=ClassId(12) index=0 value=Int(-1) occurrence=0     (… through occurrence=8)
> ```
>
> and `ClassId(12)` resolved from the layout stream, not from the record:
>
> ```
> $ CRATONVM_DBG_LAYOUT=1 cratonvm --jdk-only -cp . W37Probe
> [layout] java/lang/Class cid=12 body=136 refs=16 fields=19
> ```
>
> Nine occurrences per boot, `value=Int(-1)` every time — consistent with
> `cachedConstructor` being seeded with a sentinel rather than with a live
> `ClassId`, which is worth stating because it changes what the repair has to
> preserve.
>
> **The warning text tells the reader to do something that does not work.** It
> says "Run with `CRATONVM_DBG_TOARRAY=1` to resolve class_id to a name"; run
> with that variable set, the message is byte-identical and still prints the
> unresolved `ClassId(12)`. §8.2 already records that the warning's *advice*
> was falsified; this confirms it is still shipping in the message a reader
> hits first. `CRATONVM_DBG_LAYOUT=1` is the instruction that actually works.
> **NOMINATION**: in the `gc::guard` warning body, replace the sentence
> `Run with CRATONVM_DBG_TOARRAY=1 to resolve class_id to a name.` with
> `Run with CRATONVM_DBG_LAYOUT=1 to resolve class_id to a name.`
>
> **Scheduling: none.** The guard is a `WARN` on stderr with no assertion
> anywhere; the suite passes with nine of these per run. Nothing fails if the
> count goes to nine hundred. Record stays **OPEN**.
>
> **2026-08-12 — §8 is the first MEASURED section, and it moves this record.**
> The guard now fires on every boot. `ClassId(12)` is **`java/lang/Class`** and
> slot 0 is **`cachedConstructor`**; the store is **not a native** — it is the
> VM's own class-mirror populator in `vm/src/vm/vm_object.rs`. That falsifies
> the warning's own advice (§8.2), makes §6's population table incomplete in its
> largest row, and falsifies §4.2's "a process that never boxes" premise (§8.3).
> **Record stays OPEN**; the repair is in `vm/`, is already prescribed by
> `internal/audits/jdk-only-object-layout-audit.md`, and its own
> precondition is already discharged (§8.4).

Branch `fix/primitive-in-reference-slot-store-disagreement-20260812`.

## 1. The three behaviours, verified — and the table was one row short

The task's table reproduces. Every row was re-read rather than copied, and the
census found a **fourth** store implementation the two prior lanes did not
name.

| store | what a non-`Object` `Value` becomes in a declared-REFERENCE slot | where |
|---|---|---|
| `GenerationalHeap::set_field`, compact arm | **boxed** into a 1-field `AUTOBOX_CLASS_ID` wrapper; `get_field` un-boxes — the value survives | `gc/src/gen_heap.rs`, the `storage.is_reference()` arm; read side in the same file's `get_field` |
| `ZgcRealHeap::set_field`, compact arm | **raw 0** — the write is silently dropped to null | `gc/src/zgc.rs`, straight through to `write_compact_field` |
| `G1Collector::set_field`, compact arm | **raw 0**, same | `gc/src/g1.rs`, same delegation |
| **`Heap::set_field`, compact arm — the fourth, not in the prior tables** | **raw 0**, same | `gc/src/heap.rs` |
| any of the four, legacy 16-byte `Value` cell | the raw `Value::Int` is stored and reads back verbatim | `write_slot` / `write_value_atomic` / `ptr::write` |

The discarding arm is one line in `cratonvm_types`: `write_compact_field`'s
`FieldStorageKind::Reference` arm is
`Value::Object(Some(r)) => ptr`, `Value::Object(None) => 0`, `_ => 0`. Three
of the four heaps handed a primitive straight to it.

**Disagreements with the task's framing: one, and it matters.**
`gc/src/heap.rs`'s `Heap` is a fourth implementation of the same primitive with
the same defect. It is **not a live collector arm** — `VmHeap` has exactly three
variants (`Generational`, `G1`, `Zgc`) and nothing constructs `Heap` outside its
own tests and two doc examples — but it is a fourth place the next reader would
have to keep in step by hand, so it is converged with the others rather than
left as the odd one out. Everything else reproduces exactly, including
`gen_heap.rs`'s read-side un-box and the fact that no bogus pointer reaches any
collector on any arm: W7-69-read-side-alias-instrument.md §6's
"a pointer for the collector to mark and move" framing is wrong, and
W7-75-continuation-forkjoinpool-alias.md §4 and W7-77-guarded-slot-maps.md §4
are both right to say so.

### 1.1 The finding the two prior lanes could not see from their own row

**All four heaps already agree on auto-boxing one granularity up.**
`Heap::set_array_element`, `GenerationalHeap::set_array_element`,
`G1Collector::set_array_element` and `ZgcRealHeap::set_array_element` each box a
non-`Object` element into an `AUTOBOX_CLASS_ID` wrapper, and each un-boxes on
the read. That is not a coincidence of style: **G1's array path carried the
identical `_ => 0` encoder and the identical defect**, and its own comment
records the adjudication —

> its encoder is `Value::Object(Some(r)) => r.as_ptr(), _ => 0`, so a
> `Value::Long(42)` was written as a null reference and read back as
> `Value::Object(None)`. `.mapToDouble(...).toArray()` therefore returned all
> zeros under `-XX:+UseG1GC` and was correct under the default collector, with
> no GC involved at all.

Same encoder, same symptom, same "correct under the default collector" shape,
one granularity down. The array question was already answered, in favour of
boxing, in this file.

### 1.2 And a second agreement the compact arm was breaking

`CRATONVM_COMPACT_REF_FIELDS` is a **representation** switch — `types/src/field_layout.rs`'s
own comment says the layout must be fixed "so objects are never read under a
different layout than they were written". It is not supposed to be observable.
Before this change, flipping it on ZGC turned `Int(42)` into `null`. So the
defect was not one disagreement between collectors; it was two disagreements —
collector-vs-collector **and** compact-vs-legacy — with the same cause.

## 2. Which arms the defect actually reached

* **Compact layout is the default** (`compact_ref_fields_enabled()` returns
  `true` unless `CRATONVM_COMPACT_REF_FIELDS` is `0`/`false`/`off`/`no`), so the
  legacy arm reaches almost nothing and a fix confined to it would have been
  inert.
* **ZGC has been the default since 2026-08-10**, so the discarding answer is the
  one most code got — but only since 2026-08-10. Before that the default was
  `Generational`, i.e. the boxing arm. **Auto-boxing is the behaviour this tree
  was built and tested against for its whole history**; the null-dropping
  behaviour is two days old and arrived as a side effect of a collector default
  change, not as a decision anybody made. That is the single most load-bearing
  fact in §3.
* Mode: **both.** The defect is in `gc/`, below the Compatible/synthetic fork.
  Compatible mode (`--real-jdk`) is where it bites hardest, because that is
  where a native's slot map meets a real JDK class that declares a reference at
  that index.

## 3. The decision, and the argument

**Converge on auto-boxing.** Not on null, not on refusing, and emphatically not
on leaving them different.

### 3.1 Why not "leave them different and document it"

The standing rule — *two implementations of one primitive that disagree is the
disease* — is not the whole argument here, because the difference is also
plainly **reachable**: on the default configuration, in Compatible mode, through
a population W7-69 §4 lists and W7-75 found live. There is no unreachability
claim to make. Excluded.

### 3.2 Why not "converge on dropping to null"

It is the cheapest, and the case for it — *it makes the defect visible* — does
not survive contact with what a null actually is here.

1. **A null in a reference field is indistinguishable from a legitimately-null
   reference field.** No detector fires on it. No reader can tell. "Visible"
   would require an instrument, and if you are adding an instrument anyway you
   can add it to the value-preserving answer too — which is what §4 does.
2. **The measured downstream effect is silently wrong data, not a visible
   fault.** W7-77 §4: on a real `java.time.Month` the write nulls `Enum.name` on
   a *shared enum constant*, `getValue()` reads `Object(None)`, `.as_int()` is
   `None`, and `unwrap_or(1)` answers **January for every month of the year**,
   across `Month`, `MonthDay` and `OffsetDateTime.getMonth`. A wrong date is
   less visible than a wrong value, not more.
3. It would leave arrays and fields disagreeing (§1.1) and compact and legacy
   disagreeing (§1.2) — trading one disagreement for two.

### 3.3 Why not "converge on refusing"

This is the option with the best instinct behind it, and it is half right.

A `debug_assert!` would **red the synthetic-JDK tests**, where a fabricated
class's slot 0 genuinely *is* the primitive it is being handed — on a stub the
native's slot map is the class's own truth (W7-77 §5.1, §5.3). A hard error
would convert a wrong answer into a **crash on shipped paths**: the population
is live in Compatible mode on the default collector, and `Unsafe.setMemory` on a
non-array target writes `Value::Int` into arbitrary field slots by construction
(§6), so "refuse" means "abort on a call Java is allowed to make".

So take the **diagnostic half without the behaviour half**: the boxing path now
emits a rate-limited `cratonvm::gc::guard` record naming the class, slot and
value (§4). That is louder than what it replaces — which was nothing — and it
costs nothing, because the path it sits on already allocates.

### 3.4 Why auto-boxing

1. It is the answer **all four heaps already give for reference array
   elements**, and G1's array path reached it by fixing this exact encoder
   (§1.1). Answering the field question differently would trade a disagreement
   between collectors for a disagreement between fields and elements.
2. It is the answer the **legacy layout** has always given, in all four heaps,
   so it restores compact ≡ legacy (§1.2).
3. It is the answer **the tree's long-time default collector gave**, so it is
   the behaviour every workaround in `vm/` is written against (§3.5) and the
   configuration the suites have actually been run under.
4. It preserves the value, which is the only one of the three answers that lets
   a type-punning native's own write-then-read round-trip — and that round-trip
   is what most of these natives are doing.

### 3.5 The honest cost of choosing it

**A wrapper is non-null, so a `field == null` check that passes today under ZGC
and G1 will fail after this change.** That is real and it is the one way this
answer can make a program worse.

It is bounded, and not by hand-waving: it is exactly the pre-2026-08-10 default
behaviour, so it is the risk the tree already carried for its whole history. The
one site known to have been bitten is already guarded **collector-independently**
— `vm/src/vm/vm_init.rs` skips the `System.out` fd tag whenever slot 0 is a
compact reference field, because a non-null wrapper in `PrintStream.out` made
`route_write_through_out()` route every write into a dead wrapper (silent empty
stdout). That guard was load-bearing on `gen_heap` only and is now load-bearing
on every collector; its comment says so, in place.

The Month case changes shape rather than disappearing, and
`native-builtins/src/phases_early.rs` now says so at the site: an escape no
longer nulls `Enum.name`, so January-for-every-month is no longer the failure
mode, but the slot holds a wrapper and `Enum.name()` — real JDK bytecode reading
it as a `String` — gets an object with no class name and no methods. The
`month_slot0_is_synthetic` witness is exactly as necessary as it was. That note
exists so the next reader does not conclude from a green `getValue()` that the
guard can go.

## 4. What was built

### 4.1 `gc/src/autobox.rs` — one implementation, crate-private

Five items, and the module is `mod` rather than `pub mod` on purpose: it exists
so the four heaps cannot drift apart again, and a public re-export would invite
a fifth caller with a fifth opinion.

* `needs_reference_box(value)` — `!matches!(value, Value::Object(_))`.
  `Value::Object(None)` is deliberately **not** boxed: a native clearing a real
  reference field writes exactly that, and boxing it is precisely the §3.5
  hazard.
* `box_for_reference_slot(value, class_id, index, alloc_wrapper)` — the write
  half. Returns the value to store: unchanged when it is already a reference, a
  fresh wrapper when it is not. `alloc_wrapper` is a closure because the four
  heaps share no allocation trait at this layer and because G1 must allocate
  **before** taking its `regions` lock.
* `unbox_reference_slot(value, validated_class_id, read_payload)` — the read
  half. `validated_class_id` must prove the address is a live object of the
  calling heap before reading its header; each heap already owns that primitive
  (`is_object_address` ×3, `is_valid_heap_object` for `Heap`).
* `wrapper_exists()` / `note_wrapper_created()` — the latch, §4.2.
* `observe_primitive_into_reference_field(...)` — the instrument. Rate-limited
  to the first eight plus powers of two, the shape the sibling
  `cratonvm::gc::guard` records already use. **No `CRATONVM_*` flag of its
  own** — `tracing`'s level filter is the gate, and a new name would need
  `types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
  `docs/flag-tokens.md` and a regenerated `docs/config/flag-inventory.md` or
  `cargo test -p cratonvm-types` goes red. This is **additive** to the two
  detectors already covering the species — `vm_exec.rs`'s
  `overlay_access_is_cross_type` and `native-api`'s read-side alias census.
  Neither is quietened: both fire on the store attempt, which still happens.
  Nothing in this change makes any detector quieter.

### 4.2 The latch, and why the read path got cheaper

`gen_heap::get_field` used to pay an unconditional `is_object_address` probe
plus a header read on **every** compact reference-field read, in case the slot
held a wrapper. Giving the other three that unconditionally would have been a
real regression on the default collector.

`WRAPPER_CREATED` is a process-global relaxed `AtomicBool`, set the first time
any heap creates a wrapper — field **or** array. `unbox_reference_slot` reads it
first, so a process that never boxes pays one relaxed load of a shared,
read-only cache line and a perfectly predicted branch, and never reaches the
address validator.

**Net: no arm is slower than `gen_heap` was before this change, and in a process
that never boxes every arm is faster than `gen_heap` was.**

Relaxed on both sides is sound because the latch is a monotone one-way flag: the
only consequence of a stale `false` is one skipped un-box, and it cannot be
stale for the thread that boxed (`note_wrapper_created` runs before the wrapper
reference is published into the slot), while a reader on another thread that can
observe the slot has an ordering edge to that publication by construction.

The latch is armed by the **array** boxing sites too. Its meaning is "an
`AUTOBOX_CLASS_ID` wrapper exists in this process", which is exactly the
precondition for a read-side check to find one. Keying it to field boxing alone
would have needed an argument about which creation sites can reach which read
sites — and `Heap::get_array_element` does **not** un-box, so a wrapper genuinely
can be laundered out of an array and into a field. Conservative and needing no
such argument is worth the occasional extra probe.

### 4.3 The hot path, priced

Store side, per compact field write: `storage.is_reference()` (an enum
compare, short-circuiting for every primitive field) and, for reference fields
only, one `Value` discriminant compare. Both perfectly predicted. **No
allocation on the correct path** — `a_reference_store_never_allocates` in the
module's own tests panics from the allocator closure to pin that. G1 already
computed the discriminant compare for its barrier decision; ZGC and `Heap`
gain it.

Read side: one relaxed load and a predicted branch, plus — only in a process
that has boxed at least once — the `is_object_address` + header read that
`gen_heap` already paid unconditionally.

Allocation/GC side: unchanged except that a boxed slot is now a real reference
edge, which is what the existing oop-map machinery is for. That is the same
thing a reference array element has always been.

### 4.4 `G1Collector::get_field` split into `get_field_raw` + the un-box

The one non-mechanical edit, and it is a correctness requirement rather than
tidiness. G1's SATB pre-barrier reads the OLD slot value before overwriting it.
If that read un-boxes, the barrier logs the **primitive** instead of the
**wrapper** — and the wrapper is a real object reachable only through the slot
being overwritten, so the marker loses the edge. `get_field_raw` carries the
existing body; the trait `get_field` is `get_field_raw` plus the un-box; the
SATB block calls `get_field_raw`. This is the field half of a rule
`set_array_element` in the same file already follows: its pre-barrier reads
`old_raw` and decodes it, for exactly this reason (G1MAT-3).

`ZgcRealHeap::set_field` and `GenerationalHeap::set_field` read no old value, so
neither needed the split. `heap.rs` has no barrier.

## 5. Proving the RED

`gc/tests/primitive_in_reference_slot.rs`, seven tests, plus five in
`gc/src/autobox.rs`'s own `mod tests`.

### 5.1 The deliverable

`every_collector_agrees_on_a_primitive_in_a_reference_slot` runs the same
store/read pair under `Generational`, `G1` and `Zgc` through `VmHeap` and makes
**two** assertions:

1. the three answers are equal to each other — the half a single-collector test
   cannot express, and the half that is red **in both directions** before the
   fix (`gen_heap` said `Int`, `zgc` and `g1` said `Object(None)`);
2. the answer they agree on is the value that was stored — without which three
   collectors that all dropped the write to null would pass.

It also asserts it has at least three arms, so a `--no-default-features` build
that drops `zgc` cannot silently reduce the cross-arm assertion to a
single-collector one.

**How it fails today, per arm.** `gen_heap` → `Int(0x5EEDBEEF)`; `g1` and `zgc`
→ `Object(None)`. Assertion (1) fires naming both arms and both answers.

### 5.2 The vacuous shape it refuses, named

"the read returns something" is true of `Object(None)` (the ZGC/G1 bug), of
`Object(Some(wrapper))` (a store side that boxed while the read side did not)
and of a raw `Int` — i.e. of every state this file exists to tell apart. So
every assertion compares against an exact `Value`, and
`the_three_wrong_shapes_are_each_named` rejects each of the three by name with
the failure it corresponds to.

The sentinel is `0x5EEDBEEF`, chosen so it is neither `0` (what a
wrong-width write leaves) nor `1` (what the `unwrap_or(1)` fallbacks answer).

### 5.3 The other five, and what each stops

| test | the failure it makes impossible |
|---|---|
| `the_compact_layout_answers_the_same_as_the_legacy_cell` | compact and legacy drifting apart again — the second disagreement of §1.2, A/B'd inside one process |
| `the_two_arms_of_the_ab_are_actually_different_layouts` | that A/B degenerating into comparing one arm with itself, which would pass forever. Asserts `is_compact_object` is true for the registered-layout class and false for the no-layout class |
| `every_primitive_tag_round_trips_on_every_collector` | an `Int`-only test leaving `Long`/`Float`/`Double` unpinned — all four decode to `Object(None)` when dropped, so an `Int` case proves nothing about the others |
| `references_and_nulls_are_untouched_on_every_collector` | the §3.5 regression: a genuine reference must survive, and `Object(None)` must **not** become a wrapper |
| `the_adjacent_primitive_field_is_not_clobbered` | an 8-byte wrapper pointer written where a 4-byte `int` lives — invisible to every assertion above |
| `a_boxed_slot_survives_a_collection_on_every_collector` | the converse of W7-69 §6's wrong prediction. There was no pointer to mark because the write was being dropped; there is one now, it is legitimate, and the thing to prove is that tracing keeps it alive |

`install_layout` **asserts** rather than adapts that the compact arm is the one
running: on the legacy arm every store in the file was already
value-preserving, so a run with `CRATONVM_COMPACT_REF_FIELDS=0` would pass
without testing anything.

### 5.4 What is NOT proven

1. **Nothing was executed.** No `cargo build`, `check` or `test`. The tests'
   predicted RED per arm in §5.1 is this record's prediction, not a
   measurement. `rustfmt --check` was run on the nine edited files purely as a
   parser; all nine parse.
2. **No Java-level probe.** The observable needs a native that type-puns into a
   real JDK class, and after W7-75 and W7-77 the known live ones are repaired
   or guarded — which is the right state and also means there is no clean Java
   expression of the defect left. The Rust cross-arm test is the honest
   instrument here, and a Java probe that could not fail would be worse than
   none.
3. **No measurement of the latch.** §4.3 prices the hot path by inspection. The
   claim that `gen_heap`'s read got cheaper follows from removing an
   unconditional probe, but "not measured" is not "free".

## 6. The population that can reach this

Two numbers, and they answer different questions.

**Upper bound, mechanical:** `set_field(_, <constant slot>, Value::{Int,Long,Float,Double})`
across the native crates — native-builtins 3,012, native-io 241,
native-collections 225, native-api 4, **3,482 total**. The overwhelming majority
target objects the native allocated itself, where the fabricated class's slot
genuinely is a primitive and the store is correct.

**The audited subset — a primitive into a slot a REAL JDK class declares as a
reference.** Taken from W7-69 §4.3 and re-derived against the layouts in
W7-75 §1 and W7-77 §1:

| class | slot | native writes | real class has | status |
|---|---:|---|---|---|
| `jdk/internal/vm/Continuation` | 2 | `state` `Int` | `parent` `Continuation` | LIVE both modes — repaired by W7-75 |
| `jdk/internal/vm/Continuation` | 3 | `pin` `Int` | `child` `Continuation` | LIVE — repaired |
| `jdk/internal/vm/Continuation` | 4 | `preempt` `Int` | `tail` `StackChunk` | LIVE — repaired |
| `java/util/concurrent/ForkJoinPool` | 0 | `parallelism` `Int` | `termination` `CountDownLatch` | LIVE, default config — repaired |
| `java/util/concurrent/ForkJoinPool` | 1 | `active` `Int` | `saturate` `Predicate` | dead on the default path — repaired anyway |
| `java/time/Month` | 0 | `value` `Int` | `Enum.name` `String` | synthetic-only — guarded by W7-77 |
| `java/util/concurrent/Phaser` | 2 | `phase` `Int` | `root` `Phaser` | synthetic-only |
| `java/lang/Thread` (`THREAD_MIRROR_*`) | 2 | `tid` `Long` | `name` `String` | self-allocated receiver |

Eight rows across five classes; five were LIVE in Compatible mode until W7-75
landed today.

**And one that is not enumerable by class at all.** `Unsafe.setMemory` on a
non-array target loops `ctx.set_field(obj_ref, off + i, Value::Int(value))`
across a caller-chosen slot range (`native-builtins/src/unsafe_natives.rs`).
Any declared-reference slot in that range takes this path, on any class, by
construction — which is why "refuse" (§3.3) would mean aborting on a call Java
is allowed to make, and why a fix at the collector rather than at the call sites
is the right layer.

## 7. What this lane did not fix

1. **The JIT field helpers are a fifth reader and are collector-independent.**
   `vm/src/jit/helpers.rs`'s `jit_putfield_*` / `jit_getfield_*` compute
   `jit_field_cell_ptr` and call `read_compact_field` / `write_compact_field`
   directly, bypassing every collector. A JIT reference-field READ therefore now
   sees the wrapper on all arms (before: the wrapper under `gen_heap`, null
   under the rest) — so this change strictly *reduces* the number of disagreeing
   readers, from three to two, and the residual is interpreter-vs-JIT rather
   than collector-vs-collector. A JIT primitive *store* into a reference slot is
   only reachable on a miscompile and boxing it there would hide one; left
   alone deliberately.
2. **`gc/src/heap.rs`'s `Heap` is converged but is not a live collector arm.**
   `VmHeap` has three variants and nothing else constructs it. Whether it should
   exist at all is a different question.
3. **The `Object(None)` asymmetry is a decision, not an oversight.** A null
   store stays null rather than becoming a wrapper carrying `Int(0)`, because
   the alternative is the §3.5 regression on every reference field in the heap.
   It means a *deliberate* `Value::Object(None)` write and a *type-punned*
   `Value::Int(0)` write are still distinguishable, which is what we want.
4. **`verify_declared_slot_maps` still has no caller** — W7-69 §7.2 and
   W7-77 §7.2, unchanged. Nothing here needed it.
5. **The latch is process-global and never clears.** Once any wrapper is
   created, every compact reference-field read in the process pays the
   validated probe for the rest of its life. A per-heap latch would be tighter
   and would need a heap handle at every read site, which is not free either.
   If a profile ever names this, that is the named place to look.
   **Superseded 2026-08-12 by §8.3: the latch is armed on every boot, before
   user code, so "once any wrapper is created" is unconditional.**

---

## 8. MEASURED, 2026-08-12 — the guard fires on every boot, and the store is not a native

This is the first section of this record that rests on a run of the VM rather
than on reading source. The orchestrator ran an ordinary Java program on the
current binary (default configuration, i.e. `--real-jdk`, ZGC, compact layout)
and `cratonvm::gc::guard` emits, repeatedly, from the first milliseconds of
boot:

```
WARN cratonvm::gc::guard: a non-reference value was stored into a slot the class
declares as a REFERENCE — boxing it into an AUTOBOX_CLASS_ID wrapper …
  class_id=ClassId(12) index=0 value=Int(-1)  occurrence=0
  … occurrence=1 … 8, then 16, 32, 64, 128 (value=Int(378), Int(12), Int(164), Int(654))
```

At least 129 boxes before the workload starts. §5.4's "nothing was executed"
still applies to the tests; **this** is a measurement, and it changes three
things in this record.

### 8.1 `ClassId(12)` is `java/lang/Class`, and slot 0 is `cachedConstructor`

The identification does not need the boot class order, because the values close
the loop by themselves.

* `ClassId` is a per-`ClassStore` index handed out in load order —
  `ClassStore::next_id()` is `ClassId::new(self.classes.len())`
  (`classloading/src/class.rs:1249`), `add` appends
  (`:1257`). The `class_id` this guard prints is the **receiver's header class
  id**: `ZgcRealHeap::set_field` passes `header.class_id`
  (`gc/src/zgc.rs:5288`), and `gen_heap`/`g1`/`heap` do the same.
* `Value::Int(-1)` into slot 0 has exactly **one** producer that can be stamped
  with a `java/lang/Class` header:
  `vm/src/vm/vm_object.rs:1380`, in `get_or_create_primitive_mirror`, whose own
  doc says *"Primitive mirrors use ClassId(0) and store Int(-1) in field 0 as a
  marker"*. The mirror is allocated with
  `alloc_object(class_class_id, mirror_field_count)` where `class_class_id` is
  `cm.load_class("java/lang/Class")` (`:1326`, `:1353`).
* The remaining values are `class_id.as_u32() as i32` from the sibling
  populator, `get_or_create_class_mirror`
  (`vm/src/vm/vm_object.rs:1181-1184`):
  ```rust
  shared
      .mem
      .heap
      .set_field(mirror, 0, Value::Int(class_id.as_u32() as i32));
  ```
  **`Int(12)` in that list is the mirror of `java/lang/Class` itself** — the
  same number as the header class id of every receiver in the census. A store
  whose receiver is stamped `ClassId(12)` and whose payload is `ClassId(12)` is
  `java/lang/Class`'s own mirror, and nothing else in the tree produces that
  coincidence.

Slot 0 of a real `java.lang.Class` on JDK 25 is
`private volatile transient Constructor<T> cachedConstructor` — a reference.
This is **not a new discovery**; it is
`internal/audits/jdk-only-object-layout-audit.md` rank 6, an *overlay*
whose verdict was moved from `unknown` to `safe` on 2026-08-10, and the two
`JDK-ONLY-LAYOUT: safe` markers at `vm_object.rs:1103-1180` and `:1364-1379`
record that adjudication in place. What is new is that the W7-84 convergence
turned that overlay into an **allocation**, and that this record's own §6 census
never listed it.

### 8.2 §6's population table was missing its largest row, and the "type-punning native" framing is wrong

§6 lists eight rows across five classes and calls the writer a native
throughout; the module doc and the warning text said the same. The measured
dominant population is **`java/lang/Class` slot 0, once per class mirror and
once per primitive mirror, written by the VM core.** Corrections:

| §6 as written | corrected |
|---|---|
| the mechanical upper bound is `ctx.set_field(...)` across the native crates (3,482) | the bound is not over native crates at all. `vm/src/vm/vm_object.rs` writes through `shared.mem.heap.set_field` directly, so it is in **neither** the 3,482 nor the audited subset |
| "The store itself is a type-punning native; see the read-side alias census for which one" (the warning text) | the read-side census (`native-api/src/read_alias.rs`) observes **native reads through `NativeContext::get_field`**. It cannot see a `vm/` write, and it is a read instrument in any case. The message sent every reader to an instrument structurally unable to answer. **Corrected in `gc/src/autobox.rs` by this lane**; the replacement names the site and points at `CRATONVM_DBG_TOARRAY=1`, which prints `cid=… name=…` at exactly the mirror site and is how anyone can confirm §8.1 empirically in one command |
| five rows "were LIVE in Compatible mode until W7-75 landed today" | true and now beside the point: the live population is dominated by a row W7-75 does not touch, is not a native, and is unrepairable by any change in `native-builtins/` |

The one thing §3 gets **right** and this measurement strengthens: §3.3's
argument against `debug_assert!`/refusal. A refusal would abort every VM boot in
Compatible mode on the first primitive mirror. That is now a measured fact
rather than a prediction.

### 8.3 §4.2's performance argument does not survive — the latch is armed on every boot

§4.2 and §4.3 price the read path on the premise that *"a process that never
boxes — the overwhelming majority — pays one relaxed load … and never touches
the address validator"*, and conclude *"in a process that never boxes every arm
is faster than `gen_heap` was"*.

**There is no such process.** `WRAPPER_CREATED` is armed by the class-mirror
populator before user code runs, in every Compatible-mode run, at boot. So the
steady state is the *other* branch: every compact reference-field read in every
run pays `is_object_address` + a header read for the life of the process — the
cost `gen_heap` used to pay alone, now paid by `zgc`, `g1` and `heap` as well.

The correct statement of the change's cost is therefore:

* `gen_heap` (the pre-2026-08-10 default): **unchanged**, not cheaper.
* `zgc` (the current default), `g1`, `heap`: **strictly more expensive** on
  every compact reference-field read, plus one wrapper allocation per class
  mirror and per primitive mirror at boot.

§7.5's "if a profile ever names this, that is the named place to look" is
upgraded from a hypothetical to a standing cost. Nothing here is *measured* as a
throughput number — that needs a build and an A/B this lane cannot run — but the
premise the §4.2 argument rested on is falsified by the guard's own output.

### 8.3a §7.1's interpreter-vs-JIT residual now has a named, reachable victim

§7.1 records that the JIT field helpers (`vm/src/jit/helpers.rs`'s
`jit_getfield_*`) call `read_compact_field` directly and therefore **do not
un-box**, and argues the change "strictly *reduces* the number of disagreeing
readers". That is true as a count and it understates the consequence, because
§8.1 names the object the residual lands on.

`java.lang.Class.cachedConstructor` is read in exactly one place in the JDK —
`Class.newInstance()`, under `if (cachedConstructor == null)`. The comment at
`vm_object.rs:1104-1106` justifies the overlay on precisely that guard: *"JDK
bytecode that reads `cachedConstructor` gets an Int which it treats as an
invalid reference (effectively null) — safe because the field is always read
under an `if (cachedConstructor == null)` guard."*

After the convergence that sentence is true of the **interpreter** and false of
the **JIT**: a JIT-compiled `getfield cachedConstructor` reads the compact slot
raw and gets `Object(Some(wrapper))` — **non-null** — so the guard fails, the
fast path is taken, and `Constructor.newInstance` is invoked on an
`AUTOBOX_CLASS_ID` wrapper. Narrow (only `Class.newInstance()`, only once
compiled) but concrete, and it is a consequence this change introduced rather
than inherited: under `zgc`/`g1` the slot read back a genuine `null` on both
routes. Not measured — no build — but it follows from §7.1's own statement of
what the JIT helpers do.

If the §8.4 nomination is declined, the `vm_object.rs:1104-1106` sentence must
at minimum be corrected, because it is now the load-bearing safety claim for the
overlay and it is only half true.

### 8.4 The repair this points at, and it is not in `gc/`

The audit already prescribed it and already discharged its own preconditions.
`jdk-only-object-layout-audit.md` §4 check (c) asks whether
`mirror_class_id`'s slot-0 fallback ever fires when the reverse map is
populated, and says: *"If (c) is zero, the fix is deletion, not relocation."*
The comment at `vm_object.rs:1162-1170` records the answer — **zero fallback
hits** over five Spring Framework test classes, 87 tests, under
`CRATONVM_DBG_OVERLAY=1`. `class_mirrors_reverse` answers every runtime lookup.

So the write is preserved solely for `MockNativeContext`'s test mirrors, which
encode their `ClassId` at slot 0 and have no reverse map
(`native-builtins/src/test_utils.rs`, `vm/src/vm/tests.rs`). Deleting the two
writes and giving those mocks a reverse map removes, at a stroke: the boot-time
wrapper allocations, the unconditional arming of the latch, the interpreter/JIT
disagreement of §7.1 on the single most common object in the heap, and the
`java.lang.Class.cachedConstructor` overlay itself.

**This lane may not edit `vm/`.** The change is nominated rather than made; see
this campaign's lane report. It is a `vm/` + test-infrastructure change, not a
`gc/` one, and it is the reason this record should stay open.

### 8.5 What §8 does not establish

1. **Nothing was rebuilt or re-run by this lane.** The transcript in §8 was
   produced by the orchestrator; the identification in §8.1 is source-level
   reasoning over that transcript, and the one-command empirical confirmation
   (`CRATONVM_DBG_TOARRAY=1`, expecting `cid=ClassId(12) name="java/lang/Class"`)
   has **not** been run.
2. **Synthetic mode is unmeasured.** There `java/lang/Class` is a compatibility
   stub sized `CLASS_MIRROR_NUM_FIELDS` (2) and `resolve_class_mirror_slots`
   takes its stub arm, so slot 0 is very likely not a declared reference and the
   guard very likely does not fire. Not verified.
3. **No claim that the boxed `cachedConstructor` is observably wrong.** The
   audit's checks (1) and (2) measured the overlay as non-destructive on
   `gen_heap`, which is the arm the convergence restores everywhere. The cost
   established here is allocation and read-path cost, not a wrong answer.
