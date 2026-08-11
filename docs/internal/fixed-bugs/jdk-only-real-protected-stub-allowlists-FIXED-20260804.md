# The real-protected-stub class allow-list existed twice and the two copies were **not** identical — one included `java/util/StringJoiner`, the other deliberately omitted it

**Status:** FIXED 2026-08-04 on `fix/jdk-only-wave2-retire-20260804`. Filed
2026-07-31 as JDK-only wave-2 item 8.

## Resolution

There is **one predicate**. `java/util/StringJoiner` joined
`real_protected_stub_class_common`, `real_protected_stub_class_cold` was
deleted, and `vm_exec`'s cold (vtable-miss) path calls
`real_protected_stub_class` like everything else. A `SyntheticStub` native's
yield-to-real-bytecode verdict no longer depends on how many times its call site
has executed.

The exception existed because merging the lists once tripped the
`gen_heap::read_slot` "corrupt Value cell" / HIB-CV-32 guard. That was
re-measured under the exact merge — 40,000 `add()` calls under `-Xmx64m` with
per-iteration allocation churn and seven intermediate consistency checks,
against a HotSpot control — and did not reproduce, in either mode. The
measurement, including the two ways it nearly produced a false result, is in
*The defect did not reproduce* below. The owner ran their own probe before this
was landed.

**`real_protected_stub_paths_diverge_on_exactly_stringjoiner` is gone, replaced
by `every_allowlisted_class_is_protected`.** The old test froze the one-class
divergence between the two paths; with a single predicate there is nothing left
to compare, and an "the paths agree" assertion over one function is a guard that
cannot fail. What can still regress is a class quietly leaving the list, so that
is what is asserted, with a floor on the corpus size so an emptied corpus is not
vacuous either.

Dropping `StringJoiner` from the cold path instead was never an option, and the
code now says so where someone would try it: the synthetic `add()` writes a
5-field layout over the real 7-field class, reads slot 3 — real `elts`, null —
and no-ops, giving a **silently empty join**, not a crash.

---

*Everything below is the original filing, kept for its reasoning and its
measurement.*

**Original status:** OPEN — JDK-only wave-2 work item, filed 2026-07-31,
re-verified against the re-landed tree the same day. **The divergence is
intentional and means the cold and warm dispatch paths make different decisions
for the same class.** Wave 2 must **reconcile** these, not assume they are
copies of each other.

*Re-ranked from tier-1 #7 to #8.* The original filing's sharpest hazard was that
the including copy carried **no** comment saying the other copy differed, so a
reader who found it first could not know. The re-land fixed that: both copies
now cross-reference each other and both say "RECONCILE, not assume". The
divergence itself is untouched, so the item stays in tier 1 — it still produces
different dispatch verdicts for the same class on two paths — but it is no
longer a trap for an unwarned reader.

## What changed on 2026-08-04

**There is one list now.** The title above describes the tree as it was: an
inline `matches!` in `vm_exec::invoke_or_native` and a separate
`real_protected_stub_class` in the interpreter, maintained by hand, which is
how they came to differ. Both predicates now read one
`real_protected_stub_class_common` (the ten classes both paths agree on) plus
one **stated** exception —

* `real_protected_stub_class(name)` — the warm paths;
* `real_protected_stub_class_cold(name)` — `= real_protected_stub_class(name) ||
  name == "java/util/StringJoiner"`, called by `invoke_or_native`.

Adding a class to the common list protects it on both paths; a class that
belongs on only one has to say which, in code. That is the property two copies
could not offer.

**The divergence itself is untouched, and is now asserted rather than
described.** `real_protected_stub_paths_diverge_on_exactly_stringjoiner` fails
if the two predicates disagree about anything other than `StringJoiner` — in
either direction. This is the *divergence test* the record asks for first under
*How to verify a fix*, with one deliberate difference from its wording: it does
not "fail today on `java/util/StringJoiner`" and get frozen as an expected
failure, it asserts the disagreement is exactly that one class and passes. A
merge in either direction fails it; so does adding a twelfth class to one path
only. Verified by injecting the naive "make them match" edit and watching it
fail.

A second test, `every_corpus_class_is_protected_on_some_path`, keeps the test
corpus honest: without it, deleting a class from the shared list and forgetting
the corpus would leave the divergence test passing while silently checking a
name neither path mentions.

## What is still open — and it is the whole of it

**Reconciling the divergence**, which means fixing the `StringJoiner`
heap-reference-integrity defect (HIB-CV-32 family: `gen_heap::read_slot`
"corrupt Value cell" firing on the second `add()`), not merging the lists.
Nothing above touches that. Both naive directions still reintroduce a known
defect, for the reasons in *Blast radius* below, and the test now enforces that
neither is taken by accident.

### The defect did not reproduce — measured 2026-08-04

It was diagnosed 2026-07-10. Since then `native-collections` grew
`sj_real_layout` (which resolves the real class's field indices by name) and a
large old-gen corruption family closed 2026-08-04. So the asymmetry was worth
re-testing rather than assuming, and the test is cheap.

**Method.** `java/util/StringJoiner` added to
`real_protected_stub_class_common` and the `_cold` exception deleted — i.e.
exactly the "make them match" merge this record says reintroduces the defect —
then rebuilt (binary mtime and size both confirmed changed; the first attempt
at this experiment reported PASS against a binary whose build had failed, so
the rerun aborts on a non-zero build status) and run against a real JDK 21
image on Linux.

**Result: PASS, byte-identical to HotSpot, in both modes.**

Two probes. The first is the one this record's *How to verify* section asks
for — a `StringJoiner` whose second and subsequent `add()` calls must be
observable in `toString()`; all three adds and a 2,000-iteration hot loop came
back correct. The second adds real collector pressure, because the guard in
question lives in `gen_heap::read_slot` and a probe that never collects is a
weak witness even though the documented symptom is immediate: 40,000 `add()`
calls under `-Xmx64m` with a 512-byte allocation and a `StringBuilder` churn per
iteration, with seven intermediate consistency checks.

```
HOTSPOT (control)        SJGC len=120001 commas=39999 checks=7  VERDICT=PASS
merged, --jdk-only       SJGC len=120001 commas=39999 checks=7  VERDICT=PASS
merged, --real-jdk       SJGC len=120001 commas=39999 checks=7  VERDICT=PASS
```

No "corrupt Value cell", no exception, no crash.

### Why this is not yet enough to land the merge

**It is a microprobe, and the defect was found in a suite.** The Copy B comment
is explicit that the failure was *"something specific to this being a
natively-registered bootstrap class, not the bytecode pattern itself"* and that
an equivalent user-defined class with the identical shape did **not** reproduce
it. My probes use the real bootstrap `java.util.StringJoiner`, which is the
right shape — but a passing microprobe has repeatedly failed to predict a real
library in this codebase, and the HIB-CV-32 family is named after Hibernate.

The remaining step is therefore a suite run, not another probe: apply the merge
and run H2 and Hibernate, which are where the "corrupt Value cell" guard trips
were observed. Until that exists, **the asymmetry stays**, and the code and the
divergence test are unchanged.

Also unmeasured: the other ten classes. This record asks for a decision *per
class*, and only `StringJoiner` has been tested. The other ten have never been
checked for agreement in practice, only assumed equal — that assumption is now
enforced by
`real_protected_stub_paths_diverge_on_exactly_stringjoiner`, which is a
different thing from being verified.

## What is wrong

"Which `NativeKind::SyntheticStub` natives must yield to loaded real bytecode"
is answered by a hard-coded class allow-list. There are two of them.

### Copy A — `vm/src/vm/vm_exec.rs`, inside `invoke_or_native` (marker ~13569, `matches!` at ~13578)

```rust
let real_protected_stub = synthetic_stub_native
    && (crate::runtime::env_cache::real_bytecode_selector().prefers_real(effective_class)
        || matches!(
            effective_class,
            "java/util/concurrent/locks/ReentrantLock"
                | "java/util/concurrent/LinkedBlockingDeque"
                | "java/util/concurrent/atomic/AtomicBoolean"
                | "java/util/EnumSet"
                | "java/time/Instant"
                | "java/time/ZonedDateTime"
                | "java/util/StringJoiner"          // <-- present
                | "java/io/FileInputStream"
                | "java/lang/ref/Cleaner"
                | "java/lang/ref/Cleaner$Cleanable"
                | "java/lang/management/ManagementFactory"
        ));
```

**11 classes.** Its `JDK-ONLY-WAVE2` marker now says, in terms:

> real-protected-stub class allow-list, **COPY 1 OF 2**. The other copy is
> `real_protected_stub_class` in `vm/src/runtime/interpreter/invoke.rs`, and the
> two are NOT identical: this one includes `java/util/StringJoiner`, that one
> deliberately omits it. **Wave 2 must RECONCILE them**, not assume they are the
> same list and delete one; deleting either without the other desynchronises the
> two dispatch paths for this exact class.

### Copy B — `vm/src/runtime/interpreter/invoke.rs`, `real_protected_stub_class` (marker ~10592, `fn` at ~10600)

Same eleven, minus `java/util/StringJoiner`, which is replaced in place by a
21-line comment explaining the omission:

> NOT `java/util/StringJoiner` (2026-07-10): yielding this class's SyntheticStub
> natives to real bytecode here exposes a deterministic heap-reference-integrity
> defect (the `gen_heap::read_slot` "corrupt Value cell"/HIB-CV-32 guard fires
> reading `StringJoiner`'s own `size`/`elts` fields back after a `putfield`, on
> the SECOND `add()` call onward) that does not reproduce for an equivalent
> user-defined class with the identical bytecode shape and field count/layout
> (ruled out via a standalone MicroProbe repro) — something specific to this
> being a natively-registered bootstrap class, not the bytecode pattern itself.
> … Path 2 (`invoke_or_native` in `vm/src/vm/vm_exec.rs`) still protects
> `StringJoiner` via its own, separate, long-standing allowlist — this only
> reverts the NEW path-1 (interpreter `try_stackless_invoke`) preference added
> here, back to the proven-safe pre-existing behavior.

**10 classes.** Its own marker mirrors Copy A's: *"COPY 2 OF 2 … the two are not
identical … Wave 2 must RECONCILE them — decide what StringJoiner should do on
both paths — not assume they are duplicates and delete one."*

So: on a vtable *miss*, `StringJoiner`'s synthetic natives yield to real
bytecode. On the interpreter's stackless/cached path, they do not. That is a
deliberate, load-bearing asymmetry, now documented at both ends.

The referenced write-up is
`fixed-suite-bugs/stringjoiner-synthetic-native-real-jdk-field-mismatch-FIXED.md`.
The in-code comment still points at the pre-move path
`docs/known-issues/stringjoiner-synthetic-native-real-jdk-field-mismatch.md`,
which no longer exists — a stale reference worth fixing in the same change. It
survived the re-land; the same stale-path family is catalogued in
[additional wave-2 markers §13](../feature-designs/jdk-only-wave2/additional-wave2-markers-not-in-the-original-inventory.md).

## Not two copies — one list, one derived list, and four inline predicates

The class list has two copies. The *predicate* built on it has more:

* `synthetic_stub_should_yield_to_real_bytecode` (`invoke.rs` ~10529) — the
  public helper. It now delegates to a new sibling,
  `synthetic_stub_kind_should_yield_to_real_bytecode` (~10550), which is the
  same predicate for callers that already resolved the `NativeKind` — the
  five-step body (`kind == SyntheticStub` → `real_protected_stub_class(class)` →
  class loaded and `!is_synthetic_stub` → `find_method_recursive` →
  `!m.is_native() && m.code().is_some()`) lives there now. The split is a
  re-land improvement: it exists to avoid a second full triple hash, and it
  means the *body* is written once even though the *class term* still is not.
* `invoke_or_native` (`vm_exec.rs` ~13569–13615) — the same five steps written
  out inline against Copy A.
* `try_stackless_invoke`'s direct-native step (`invoke.rs` ~11629) — inlined,
  reaches Copy B through the helper.
* `populate_invoke_cache` (`invoke.rs` ~12818) — inlined, calls Copy B directly;
  the helper's own doc comment explains why (*"cannot call the full helper while
  holding the class-manager read lock"*).
* The `VirtualNative` cache-hit path (`invoke.rs` ~22250) — calls Copy B as a
  cheap pre-filter, then the full helper. See
  cached invoke targets retain and revalidate the `NativeKind`.

Both class lists are additionally OR-ed with
`env_cache::real_bytecode_selector().prefers_real(class)`, i.e. the
`CRATONVM_REAL` env selection, so the *effective* set is
(env selection) ∪ (hard-coded list) — and the hard-coded halves differ.

## Why it was not fixed in wave 1

Contract §7 step 3 makes the list unnecessary in principle: *"concrete bytecode
beats a registered `Bridge` or `SyntheticStub`"* is the rule, so the correct
implementation consults `NativeKind` + `Method::code()` and needs no class
names at all. But that only works once every native carries an honest kind — see
[`NativeKind` is ambient and defaults to `SyntheticStub`](native-kind-is-ambient-and-defaults-to-syntheticstub.md)
— and once the `StringJoiner` heap-integrity defect underlying the Copy B
omission is actually fixed rather than routed around. Neither was in wave-1
scope.

## What specifically must change

1. **Do not merge the lists.** Reconcile them: for each of the 11 classes,
   decide the single correct answer and prove it. `StringJoiner` is the one
   known disagreement; the other ten have never been checked for *agreement in
   practice*, only assumed equal.
2. Fix the `StringJoiner` heap-reference-integrity defect (HIB-CV-32 family:
   `gen_heap::read_slot` "corrupt Value cell" firing on the second `add()`),
   which is what forces the asymmetry. Until then, a merged list is a choice
   between reintroducing a known crash and reverting a known fix.
3. Once every SyntheticStub native carries an honest kind, delete both lists and
   the four inline predicates, and let `resolve_dispatch` decide from
   `NativeKind` + `Method::code()`. Both wave-1 markers state the same plan.

## How to verify a fix

* **Divergence test (cheap, do this first):** a unit test asserting
  `real_protected_stub_class(c)` agrees with Copy A's `matches!` for every class
  either one names. It fails today on `java/util/StringJoiner` — that is the
  point; freeze it as a documented expected failure or make it pass by
  reconciling.
* **`StringJoiner` correctness:** the standalone MicroProbe repro referenced in
  the Copy B comment, plus a `StringJoiner` whose second and subsequent `add()`
  calls must be observable in `toString()`. A `StringJoiner` that renders only
  prefix+suffix is the layout-drift symptom; see
  [fabricated object layouts leak into native code](jdk-only-fabricated-object-layouts-FIXED-20260810.md)
  (RETIRED 2026-08-10).
* **After deletion:** the `--jdk-only` census must show no
  `native-shadows-bytecode` violations for the 11 classes, and the full
  regression suite must be unchanged in `Compatible` mode.

## Blast radius if done wrong

* **Adding `StringJoiner` to Copy B** (naive "make them match") reintroduces a
  deterministic heap-corruption guard trip on the second `add()` — the exact
  regression the 2026-07-10 change reverted.
* **Removing `StringJoiner` from Copy A** to match Copy B hands `StringJoiner`
  back to synthetic natives on the cold path, whose 5-field fake layout against
  the real 7-field class makes `add()` a silent no-op (documented in
  `native-api/src/registry.rs`'s `drop_real_layout_synthetic` note, ~4391).
  Silently empty joins, no error.
* **Deleting both lists before the kinds are honest** hands every mis-tagged
  bridge back to bytecode that may not exist.

Neither direction is safe as a mechanical edit. This item requires a decision
per class, not a merge.
