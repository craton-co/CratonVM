# Lane T — the cross-cutting registrars

> **RETIRED 2026-09-10.** The lane is closed: all 57 cross-cutting registrars
> are classified, 906 triples are retired, and the two defects the arm had to
> find first are fixed. The measurements, the four blocked groups with their
> blockers, and the corrections this page needed are in
> [`../jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md`](../jdk-only/lane-t-the-throwable-family-retired-and-the-three-defects-the-arm-had-to-find-first-20260910.md).
>
> **Two things below are WRONG and the record says so with the census that
> settles them.** §1's row list and §5 assign
> `native-io/src/concrete_receiver.rs:185` to this lane; its 191 goal rows are
> all under `sun/nio/`, so lane 0 §2's own rule puts it in **L4**, and lane 0's
> 1,100 total excludes it. And §1's `lib.rs:42662`/`42668` line numbers had
> already drifted to `42649`/`42655` when this page was written — regenerate,
> as §3 says, rather than reading the snapshot.
>
> Kept verbatim below the line: the reasoning is what the record is a reply to.

---

**Scope: 1,100 §1.4 shadows over 87 classes, from 57 registration call sites.**
The largest single lane in the campaign, and the only one whose ownership is not
a class-name prefix.

Read [`lane-0-integration-and-gates.md`](../../known-issues/jdk-only-lanes/lane-0-integration-and-gates.md) §2-§6 first: the ownership table, the
shared cells, the build queue, and the merge protocol. The method, the four
retirement preconditions and the landing protocol are in
[`../jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md).

---

## 1. Why this lane exists

A class-parameterised registrar registers one method surface over a *list* of
classes. `native-builtins/src/lang_misc.rs:3416-3568` is the extreme case:
`register_throwable_subclass_natives` loops `THROWABLE_FAMILY_CLASSES` — 61
classes — registering roughly twelve methods each.

```text
 158 rows / 60 classes   lang_misc.rs:3416   the per-class constructor table
  61 rows / 61 classes   lang_misc.rs:3503   getMessage
  61 rows / 61 classes   lang_misc.rs:3510   getLocalizedMessage
  61 rows / 61 classes   lang_misc.rs:3517   printStackTrace()V
  61 rows / 61 classes   lang_misc.rs:3524   printStackTrace(PrintStream)
  59 rows / 59 classes   lang_misc.rs:3544   getCause
  61 rows / 61 classes   lang_misc.rs:3550   initCause
  61 rows / 61 classes   lang_misc.rs:3556   addSuppressed
  61 rows / 61 classes   lang_misc.rs:3562   getSuppressed
  61 rows / 61 classes   lang_misc.rs:3568   getStackTrace / setStackTrace
  53 rows / 53 classes   lib.rs:42662
  53 rows / 53 classes   lib.rs:42668
 191 rows / 21 classes   native-io/src/concrete_receiver.rs:185
```

Those 61 classes span **seven** other lanes' prefixes: `ClassCastException` is
L0's, `ExecutionException` and `RejectedExecutionException` are L5's,
`IOException` is L4's, `SSLException` is L6's, `IllegalArgumentException` is
L2's. A prefix split would put eight lanes inside one `for` loop, each measuring
one-seventh of a single behaviour change.

**So the unit of work here is the registrar, retired whole.** While this lane
holds a registrar, no prefix lane may retire any triple it produces — including
triples inside that lane's own prefix set. Announce a hold by naming the call
site in your lane page before you start.

## 2. Not yours

- Single-lane registrars inside these same classes. `java/lang/Throwable`'s own
  hand-written registrations are L2's; only the *parameterised* surface is yours.
- The three ratchet constants, the `triple_is_retired_shadow` chain, the prefix
  list, the kind-map header. See L0 §4.
- `java/lang/Class.getName` and `getModule` — already resolved as reviewed
  `Intrinsic`s by L0.

## 3. The trap that will cost you a wave if you skip it

**A class-parameterised registrar is invisible to the source-scanning drift
gate.** That gate counts `register(` call sites in the source. Sixty-one rows
behind one call site read as *one* row — and when a helper refactor removes
rows, the gate reports the loss as **good news**.

Two consequences:

1. **Never score a movement in this lane with the source scanner.** Use the
   paired registry census: dump `--dump-native-registry --explain-jdk-only`
   before and after, and diff the row sets. A source-scanning gate reads the
   tree at runtime, so one prebuilt binary can score two revisions — you do not
   need a build per side.
2. **Flipping a `set_category` line from `Bridge` to `SyntheticStub` takes a
   whole registrar out of the `Bridge` population**, so `bridge-ratchet.sh`'s
   numbers *fall* and it prints "IMPROVED — lock it in". That is the shape of
   the 2026-07-14 `java.util.Properties` regression reading as a win. The
   kind-map freeze exists precisely because this ratchet cannot see the
   dangerous direction. Amend kind-map rows for every triple you move.

Note the registrar already brackets itself with `r.set_category(Bridge)` /
`r.set_category(__prev_cat)`. Kind is **ambient**: changing that bracket changes
all 61 classes at once, which is convenient and is exactly why it is dangerous.

## 4. First target: the throwable family, and it is not obvious

The ~730 throwable rows are bucket **B** — `getMessage`, `getCause`,
`toString`, `addSuppressed` and the rest are inherited from `java.lang.Throwable`,
which carries real `Code`. So §1.4 applies and the remedy — yield to
`Throwable`'s bytecode — is *available*. But three things must be settled first,
and the registrar's own comments name two of them:

- **The constructors are not interchangeable with the accessors.** The comment
  at `lang_misc.rs:3416` records that four hard-coded descriptors per class were
  once registered for every class in the list — "103 descriptors the real JDK
  class does not declare and 16 it does that nobody registered". Constructors now
  come from `cratonvm_classloading::throwable_ctor_descriptors`, shared with the
  synthetic stub's table so the two cannot disagree. **Retire accessors and
  constructors as separate waves**, and treat that shared table as a third
  consumer you must not break.
- **`cause` has a sentinel.** audit-2026-05-16 found a generic
  `native_noop_with_this` behind those constructors left `cause`
  un-initialised, so `initCause()` succeeded after a `(String)` constructor and
  failed after the no-arg one. Real `Throwable` writes `cause = this` as the
  "not yet set" sentinel. **Any probe here must exercise `initCause` after each
  constructor overload separately** — this is a defect a single-constructor
  probe cannot see.
- **`getStackTrace` is a VM-filled field.** `Throwable.backtrace` is written by
  the VM at `fillInStackTrace` time. If yielding gives an empty or wrong trace,
  that row is a reviewed-`Intrinsic` candidate (L0 §7), not a retirement — and
  the two frame-walk APIs in this VM **order their results oppositely**
  (`capture_stack_trace` is outermost-first, `frame_class_ids` innermost-first),
  which is the most likely way to get this subtly wrong.

A single probe covering the twelve accessors times the four constructor shapes
times a few representative classes from *different* lanes' prefixes is the
review for the whole wave. One build scores it.

**Do not let the loud row hide the silent ones.** A probe section that aborts on
the first throw hides every quiet wrong answer behind it; run each row
independently and print all of them.

## 5. `concrete_receiver.rs:185` — 191 rows, 21 classes, and a different story

The second-biggest site is the channel surface: `read` (13), `write` (13),
`open` (12), `close` (11), `isOpen` (9), `provider`, `setOption`, `bind`,
`accept`, `select`, `poll`. Its classes are L4's and L6's
(`sun/nio/ch/SocketChannelImpl`, `DatagramChannelImpl`,
`ServerSocketChannelImpl`).

Two warnings specific to it:

- `sun/nio/ch/` as a package is **on record as not retirable** — it scored 34/36
  on the 2026-08-19 dial sweep, and the prefix list carries a note saying so.
  Phase 2 narrowed that to exactly one triple, measured on its own. Treat a
  package verdict as the default and beat it per-triple or not at all.
- These are real I/O paths. A retirement that turns a loud failure into a rare
  silent one is not a retirement — and this lane has the worst version of that
  risk, because a wrong `read` return value corrupts data instead of throwing.

## 6. The increment loop

1. Pick a registrar. Name it in this page as **held**.
2. Dump and funnel: for every triple the registrar produces, check it owns its
   slot, its kind is `Bridge`, its image target carries `Code`, and it was
   dispatched by **your own** instrument (`invocations > 0`). Precondition 4 is
   per-instrument; a corpus census cannot answer it for you.
3. Write the probe. Capture the HotSpot oracle. Neither needs a cratonvm build.
4. Fill `RETIRED_SHADOW_LT_TRIPLES` — sorted, unique, and within a prefix that
   is already in `RETIRED_SHADOW_PREFIXES` (L0 pre-populated them; an entry
   outside every prefix silently answers "not retired").
5. Take the build token. Build once for the whole wave.
6. **Prove it is not inert.** A refusal is a retirement only when nothing else
   already owns the triple: check
   `JdkOnlyViolation::SyntheticNativeRegistered.survivor`. Report refusals *and*
   survivors, as "N refusals, 0 survivors".
7. Measure: probe-tree A/B against the pre-change binary, `--jdk-only` corpus,
   `SUITE=all` at `TIMEOUT=600`, and the `all`-arm count.
8. Gate set per the ops page §5. Amend kind-map rows. Commit. Do not push.

## 7. Done

Every one of the 57 cross-lane registrars is classified in this page as:
retired (with the wave that did it), reviewed `Intrinsic` (with the probe),
correct as-is under §1.5, or blocked with the blocker named. Report
`before -> after` on bucket-A/B rows for your registrars, and the survivor
count beside it.
