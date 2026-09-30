# Round 10, lane `diagread` — proposals

Written 2026-09-21 while closing
`r10-gcs2-implicit-null-diagnostic-counters-orphaned`, reviewing
`jit/src/implicit_null.rs`, and building
`scripts/check-orphan-instruments.sh`. Everything below is grounded in
something read in this lane; nothing here was measured by running it, because
this lane was not permitted to build or run the tree. Where a proposal turns on
a number, the number is a count from a search and is labelled as one.

---

## 1. Per-registration retirement: give `register` a slot handle

**Status of the idea:** already written down as item 2 of the `REVIEW-NOTE` at
the foot of `jit/src/implicit_null.rs`, owner unassigned. Raising it here
because the round-10 retirement move changed the argument for it.

Retirement is currently per-*page*: `register` pushes its slot index onto a
chain keyed by `fault_pc >> 12`, and `unregister_range` walks the chains of the
pages the range covers. The stronger design is per-*registration*: `register_at`
returns `Option<u32>`, the driver collects the handles into a small `Vec<u32>`
on the `CompiledMethod`, and retirement retires exactly those slots. That is
O(sites in this method), with no chain walk, no dead nodes accumulating on a
hot page's bucket, no `MAX_RANGE_PAGES` fallback, and no striped mutex.

**What round 10 changed.** The retirement call moved from
`CompiledMethod::drop` to `ExecutableBuffer::drop` (`33957575f`), and it now
retires the whole mapping, `[ptr, ptr + capacity)`, not `[entry, entry + pos())`.
Two consequences:

* The per-page walk now covers *capacity*, which is the arena's rounded-up block
  size rather than the emitted length — strictly more pages per drop. The chains
  absorb this easily today, but the proposal's advantage grew rather than shrank.
* The handle would have to live on whatever owns the *mapping*, not on the
  `CompiledMethod`, or the two would come apart again in exactly the way this
  round had to fix. That is a genuine complication the original note did not
  have to consider, and it is the main reason not to take this on speculatively.

**Verdict:** still worth doing, still not worth doing *first*. Take it when a
profile shows retirement cost, which the page chains make unlikely below heavy
same-address recompilation. If it is taken, `unregister_range` stays as the
belt-and-braces sweep — it is what makes the mapping-wide guarantee
independent of any bookkeeping the compiler might get wrong.

---

## 2. Teach the orphan-instrument gate the difference between a wrapper and a caller

`scripts/check-orphan-instruments.sh` calls a reference in the defining file
"not a caller" and everything else a caller. That is deliberately crude and it
costs precision at both ends:

* **False positive:** a `pub fn` only its own file calls is flagged, though the
  defect is over-broad visibility rather than a missing reader. A visible share
  of the 112 frozen entries are this.
* **False negative (limit L2):** an instrument called only from a
  `#[cfg(test)] mod` in a *different* production file counts as called.

Both are fixed by the same machinery: the column-0 `#[cfg(test)] mod` block
scanner already written and debugged in `scripts/untyped-alloc-ratchet.sh` (it
measured 54.9% of that gate's sites as test-module sites, and its v7a bug —
`pub(crate) mod new13_tests {` not matching — is exactly the kind of thing worth
inheriting rather than rediscovering). Lift it into a shared
`scripts/internal/rust-test-blocks.awk` and have both gates use it.

Adding a *severity* split at the same time would make the allowlist readable:
`fn` (no caller anywhere), `fn-local` (only its own file), `fn-test` (only test
code). Only the first has to block; the other two can be reported and ratcheted
without failing the build. A gate people read is worth more than a gate that is
merely correct.

---

## 3. Make "this instrument has a reader" a compile-time property, not a grep

The gate is a grep and says so. The stronger version, for *new* instruments, is
structural: make a counter's reader impossible to omit.

The mechanism already exists in this tree, in `scripts/baselines/README.md`:

> "A baseline these files' consumer does not read is a file that cannot go red.
> `jdk_baseline.rs::ALL` is compared against `read_dir` of this directory in
> both directions, so a `jdk25-*.tsv` added here without an `include_str!` fails
> the build's tests rather than sitting unread."

The same trick applies to counters. A module that declares its instruments in
one array —

```rust
pub const IMPLICIT_NULL_ROWS: [&str; 7] = [ "registered", "retired", ... ];
pub fn rows() -> [(&'static str, usize); 7] { ... }
```

— can have a `#[test]` that asserts every row name appears in the shutdown
dump's format string (read with `include_str!`, the way
`the_method_entry_is_still_the_buffer_base` already reads `x64/driver.rs`). A
counter added to the array without a line in the dump fails a test on the edit,
which is strictly better than failing a grep on the next CI run, and *much*
better than failing nothing for six months.

Scope it to the modules that already have a row table (`jit/src/metrics.rs`'s
four `*_EVENTS`, `implicit_null`'s five counters) rather than the whole tree.

---

## 4. A `--why` mode for the gate, because a name is not a finding

Today a failure prints a name. A reviewer then has to run the searches by hand
to decide whether it is a real orphan, a file-local helper, or a dynamic-
dispatch false positive — which is the work the gate was supposed to save.

`--why <name>` should print, for one candidate: its definition site; every
textual reference with its file, line and classification (definition / prose /
same-file / test-dir / caller); and, when there are no callers, the nearest
*wired* sibling in the same file, with its caller. That last one is the whole
argument in two lines — it is exactly how `jit_corrupt_value_cells` was shown to
be real (its two siblings in `vm/src/jit/helpers.rs` are both printed by
`vm-cli`, and it is not).

Cheap: every piece of data is already in the script's temporary files.

---

## 5. Retire `CRATONVM_PRECISE_COVERAGE_PIN`'s counter, or wire it

Found while spot-checking the census.
`vm/src/jit/conservative_roots.rs:8243`'s `uncovered_precise_frame_count()`
counts "precise frames observed NOT `fully_oop_covered` during GC scans while
`CRATONVM_PRECISE_COVERAGE_PIN` is on", and its own doc says "0 when the knob
was never on". Nothing reads it.

So the knob's entire output today is rate-limited stderr logging capped at
`STEP3_LOG_CAP = 64`. A run that turns the pin on and produces 10,000 uncovered
frames prints 64 lines and reports the same total — none — as a run that
produced zero. A diagnostic knob whose summary statistic is unreachable is worse
than no knob, because someone will turn it on, read 64 lines, and conclude.

Either wire the counter into the GC summary print (it is one line beside the
figures `claim_gc_summary_print` already emits) or delete the counter and the
static and say in the commit that the log lines are the whole instrument.

---

## 6. Two doc-level follow-ups in `jit/src/implicit_null.rs` this lane did not take

Both are noted here rather than done, because both touch files this lane does
not own.

**(a) `exec_memory.rs:735`'s comment is now stale in the same way the module
header was.** It says "`CompiledMethod::drop` retires this range too", and since
`33957575f` it does not — that call was deleted in the same commit that added
this one. The comment's *argument* survives (retiring at the mapping is correct
because retiring at the owner is too early); only the "too" is wrong. Whoever
owns `jit/src/exec_memory.rs` should drop that clause. Confirm with
`grep -n 'retires this range too' jit/src/exec_memory.rs` — one hit,
`exec_memory.rs:735` — and
`grep -n 'implicit_null::unregister_range' jit/src/lib.rs` — one hit, and it is
a doc comment at `lib.rs:15539`, not a call. `jit/src/lib.rs` has had no call to
it since `33957575f`.

**(b) `x64/driver.rs:2898`'s block comment argues the pre-move invariant.** It
says the entry must be the buffer base "because `CompiledMethod::drop` retires
`[entry, entry + buffer.pos())`", and then that a moved prologue means "every
site below it silently stops being retired". Neither half holds now: the drop
does not retire, and the mapping-wide range covers a moved entry's sites anyway.
The check itself should stay — `note_registration_base` is now the only
assertion that registration keys off an address inside the mapping that will be
swept, and the OSR-trampoline purge still reads `entry` — but its stated reason
should be replaced with that one. `jit/src/implicit_null.rs`'s module header and
its `the_method_entry_is_still_the_buffer_base` failure message were corrected
in this lane's change and can be copied verbatim.

---

## 7. A `CRATONVM_DBG_IMPLICIT_NULL_HEALTH` sampler, and the argument against it

The obvious next step after wiring the three health counters into the shutdown
dump is to sample them *during* a run, so a server that fills its table at hour
six is visible at hour six rather than at teardown.

Written down mainly to argue the opposite. `active()`'s headroom gate already
turns the feature off before the table can decline, the driver already refuses
an artifact with a declined site, and a periodic sampler is a thread, a timer
and a place for a lock to appear near a module whose defining constraint is that
its read path takes none. If the pressure is real, the cheap form is a one-shot
`tracing::warn!` the first time `has_headroom()` returns false — no thread, no
timer, one `AtomicBool`, and it fires at the exact moment the feature turns
itself off, which is the only moment worth a log line.

That is a change to `implicit_null.rs` alone and would fit in a future wave.
