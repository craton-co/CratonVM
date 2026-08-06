# L11 — Items 3 + 7: delete the hard-coded lists — **item 3 DONE 2026-08-04**

**Owns:** `vm/src/runtime/interpreter/native_override.rs`,
`vm/src/vm/vm_exec.rs` (dispatch regions ~14700 and ~22700)
**Gated on:** ~~**L9** (String)~~ — L9 is closed, and item 3 is done with it;
see the outcome record (`forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md`).
**L10** (ThreadPoolExecutor) still gates item 7. Do not start item 7's deletion
before it lands — both naive directions have already reintroduced known
defects.

## Item 3 is done, and the plan below was wrong about how

Steps 1 and 3 below assumed the `String` lists were load-bearing and that
deleting them would change dispatch. **They were inert.** A binary with both
deleted produced a byte-identical 392-case `String` transcript in both modes and
identical invocation counts on all 38 exercised registry slots, because
`resolve_step1_native` dispatches a registered native on the triple alone,
before any list runs. Deleting the lists was therefore free — and, on its own,
achieved nothing.

What achieved something: moving the decision to REGISTRATION.
`NativeMethodRegistry::register` now drops every `java/lang/String` `Bridge` in
real-JDK mode, which is invisible to every dispatch path at once. That is the
literal meaning of this doc's "replace both with `resolve_dispatch`" once you
know the lists were never the gate. Four copies went, not three — the fourth,
`is_jdk_string_charset_name_constructor_override`, was uncounted by the record.

Read step 5's caution before doing item 7 the same way: it is right that this
routes more traffic onto §7 step 3, and `resolve_step1_native`'s
`bytecode_available: false` is still open.
**Conflicts:** L4 also edits `vm_exec.rs`, in the hunter region (~3070–3200).
Disjoint regions, one file: coordinate, never `git add -A` blind.
**Effort:** M once unblocked

## Goal

Delete the three hard-coded policy lists and let `resolve_dispatch` decide from
`NativeKind` + `Method::code()`:

* item 3 — the forced-native `String` policy (21-name positive list, 7-pair
  exclusion, JIT ladder);
* item 7 — the eight `ThreadPoolExecutor.execute` receiver-shape sites plus the
  ninth unconditional `force_native` arm.

## What is already done, so you do not redo it

**Item 3.** The statically-unreachable h2-bnf block is alive (a landed, measured
fix that had never once executed). Both halves of the policy are named functions
side by side rather than one function and one inline `matches!`. A 29-shape
table pins the `(cold, warm)` verdict pair, verified by injection.

**Item 7.** A census constant names all eight sites plus the ninth. The
receiver-shape probe has one implementation instead of three. A gate fails on a
partial sweep — so a half-done deletion is caught rather than shipped.

**Item 8 is closed** and is the template: the two real-protected-stub allow-lists
were reconciled into one predicate on 2026-08-04, after the defect that forced
the divergence was re-measured and did not reproduce.

## The trap, in both directions

From item 8's history, which cost a session: *both* naive directions reintroduce
a defect. Merging the lists reintroduced (or was believed to reintroduce) a heap
guard trip; deleting the entry from the other side turned a `StringJoiner.add()`
into a silent no-op that renders an **empty join** rather than crashing.

So: do not delete a list because it looks redundant. Delete it because the
condition that required it is provably gone — which is what L9 and L10 deliver.

## Steps

1. Confirm L9's exit criterion: the `String` lists removed, strict boot clean.
2. Confirm L10's: `threadpool_executor_has_real_workers` true for every executor
   the factories produce.
3. Delete item 3's lists; run the 29-shape verdict table — it should now be
   uniform, and if it is not, the deletion is premature.
4. Delete item 7's eight sites and the ninth arm; the partial-sweep gate must go
   green *because there is nothing left to sweep*, not because it was relaxed.
5. Replace both with `resolve_dispatch`. §7 step 3 already fires ~3,344 times per
   short run, so this path is exercised — but see the caution below.

## Caution on §7 step 3

That path had a defect until 2026-08-04: its decline fell through to
`UnsatisfiedLinkError` rather than to the bytecode, because the `is_native` arm
has three outcomes and none of them is the bytecode. `--jdk-only` could not start
a thread. It is fixed, but this lane routes *more* traffic onto that path than
anything before it, so re-read
`jdk-only-section7-step3-unsatisfiedlinkerror-FIXED-20260804.md`
before trusting it.

## Verification

* Both probes vs HotSpot, both modes, exit status checked.
* The 29-shape String verdict table.
* The eight-site census gate.
* `Compatible` byte-for-byte against the pre-fix binary over `test_classes`,
  timestamps normalised. Contract §5 — and most of the dangerous mistakes
  catalogued in this feature were `Compatible` changes made while intending to
  fix strict mode.
* A full suite run. These lists are load-bearing for real boots today.

## Done when

Both lists are gone, `resolve_dispatch` is the only decider, and the suites are
no worse than before.
