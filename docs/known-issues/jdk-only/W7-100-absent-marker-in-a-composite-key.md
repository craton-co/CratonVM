# W7-100 — one missing feature inside a composite harness key produces a sweeping DIVERGE verdict

Status: **METHOD RECORD, not a VM defect.** The VM defect it is derived from is
`W7-92-shutdown-hooks-never-run.md`. Filed 2026-08-12 by lane C9 because the
same shape has now cost this campaign three lanes and 57 findings, and because
the diagnosis is cheap once you know to ask for it.

---

## 1. What happened

A corpus harness built a per-class result key by concatenating several markers,
one of which was `completed=`, printed from a `Runtime.addShutdownHook` hook.
CratonVM registers shutdown hooks and never runs them, so `completed=` never
appeared, so the key never matched, on **every class in the corpus**.

Three separate lanes each read that as a cross-VM **DIVERGE** verdict against
the VM, at 12, 9 and 36 findings. **One gap wore fifty-seven hats.** None of the
three verdicts was about the thing it named.

## 2. The two shapes, and why they are one

**Shape A — a conjunctive key with a structurally impossible conjunct.** A key
of the form `A && B && C` is a measurement of `min(A, B, C)`. If `C` is a
feature the subject has never implemented, the key answers "different" for every
input and the harness reports a difference *per input*, which reads as breadth.
The count is the corpus size, not the defect count. **A finding count that
equals the corpus size is the signature.** Check for it before reading any row.

**Shape B — "ran but its output was lost" is indistinguishable from "never
ran".** Both produce an absent marker. This is what makes shape A so expensive:
the natural next question ("did the hook run?") has no answer from the evidence
in hand, so the investigation goes to the *contents* of the diff rather than to
the *mechanism of the key*.

They are one defect because the harness's observable was a conjunction whose
failing conjunct was unobservable. Fix either half and the error does not happen.

## 3. The instrument — separate the two states BEFORE reading any row

A probe for any "did X happen at teardown" question needs three things. All
three are in `scratchpad/c9/ShutdownProbe.java` and in
`regression-suite/src/RShutdownHooks.java`:

1. **A reached-here marker printed by the code that is supposed to schedule the
   work**, before it leaves. `MAIN-END <mode>`. Without it, "the hook did not
   run" and "the program never got that far" are the same observation and the
   second is a completely different defect.
2. **The report written on a channel the failure mode cannot eat.** The hook
   writes to `System.out`, to `System.err`, and to a raw
   `FileOutputStream(FileDescriptor.out)` + `flush()`, and it reports the result
   of all three **on the raw channel**. If the hook runs on a VM whose
   `System.out` is dead or unflushed at shutdown, the raw line still lands
   carrying `out=LOST`; if the hook never runs, no line lands at all. Two
   states, two observations. Putting the report on `System.out` reintroduces the
   exact ambiguity.
3. **A negative arm.** A hook that was registered and then removed must NOT run.
   Without it, a VM that started every `Thread` it had ever seen satisfies the
   positive arm.

Mutation-checked on HotSpot 25.0.3+9: the four states (correct / never-runs /
removed-hook-runs / ran-but-stdout-dead) produce four distinct outputs after
`regression-suite/run.sh`'s own `extract()` filter. See W7-92 §5.

## 4. The cheap first question, for any sweeping harness verdict

Before reading the rows of a harness that reports a large, uniform difference:

* **Is the finding count equal to the number of inputs?** If so, suspect the
  key, not the subject.
* **Is any component of the key produced by a VM feature rather than by the
  application under test?** Shutdown hooks, finalizers, `Thread.exit` cleanup,
  JFR events, agent premains, `Cleaner` callbacks, `atexit`. Each of those is a
  VM capability that a harness author reaches for as if it were application
  code.
* **Print the raw, unfiltered output of ONE input on both arms and diff it by
  eye.** The three lanes here all worked from the harness's own summary. The raw
  transcript of a single class would have shown `completed=` missing on one arm
  and present on the other, with everything else identical, in one command.
* **Grep the subsystem's statics for a write-only one.** W7-92's whole
  diagnosis is that `SHUTDOWN_HOOKS` has a pusher, a remover, and no reader.
  That is a fact about the tree, available without running anything.

## 5. The adjacent trap this record does NOT claim

"Fails on HotSpot too" clears the VM only for the harness **as configured**. A
harness gap can mask a VM defect as well as inflate the list — so discovering
that a key is broken does not retire the findings it produced, it un-adjudicates
them. The 57 findings above are **unknown**, not **cleared**. They have to be
re-run once W7-92's fix lands, against a key that no longer depends on it.

## 6. Cross-references

* `W7-92-shutdown-hooks-never-run.md` — the VM defect, the HotSpot oracle for
  all five exit paths, and the nominations.
* `W7-60-harness-extract-blindness.md` — the sibling shape on the other side:
  `run.sh`'s `extract()` deletes every line not prefixed `PASS `/`CK `, so a
  vector can print 43 lines of evidence and have 1 reach the diff. Subtract the
  filter from the oracle before believing either.
* `W7-51-vacuous-sweep-round-2.md` — the same family in the tests rather than
  the harness, plus §7's positive-control rule.
