# H1-1 — the sink that capped every count, and a P0 row that expired two weeks ago

**Status:** **FIXED-UNVERIFIED** — no binary carrying these changes has been
built or run. Nothing below is an "after" measurement, because there is none.
Every forward-looking claim is labelled **PREDICTED** and carries what would
falsify it.

> **VERIFIED AGAINST A BINARY 2026-09-02.** The banner says "no binary carrying
> these changes has been built or run" and that every forward-looking claim is
> PREDICTED. A `--jdk-only-report` from a build of this tree now carries the
> shape §1.3 predicted:
>
> ```json
> "observation_sink": {
>   "recorded": 61, "cap": 4096, "saturated": false,
>   "truncated": false, "dropped": 0,
>   "jit_fastpath": { "recorded": 0, "cap": 4096, "truncated": false, "dropped": 0 },
>   "jit_compile":  { "recorded": 0, "cap": 256,  "truncated": false, "dropped": 0 }
> }
> ```
>
> `truncated`, `dropped` and `saturated` are all present, with both per-source
> sub-objects. The `vm-cli` half is there too — a `--jdk-only` suite run prints
> *"saturation: none — every bounded collection reported `truncated: false`, so
> the counts above are totals, not floors."*
>
> **One field differs from the prediction, and the difference is the interesting
> part.** §1.4 — "Why `jit_compile` is `null` and not `false`" — deliberately
> reported `null` because that lane did not own `jit/src/lib.rs` and **"an
> unmeasured thing must not render as a clean one"**. Today it reads `false` /
> `0`. That is exactly the shape §1.4 was guarding against, so the VALUE alone
> cannot say whether the guard was honoured or lost.
>
> The mechanism says it was honoured: §5's patch landed.
> `record_jdk_only_direct_native_refusal` in `jit/src/lib.rs` now increments
> `JDK_ONLY_VIOLATIONS_DROPPED` on the at-capacity branch, with a comment naming
> the same reordering — dedup BEFORE the capacity test — that `helpers.rs` got on
> 2026-08-20. The counter exists, so `false` / `0` is a measurement rather than a
> default. **A changed value has to be checked against its mechanism, not
> accepted**: had the `null` been replaced without the counter, the report would
> read identically and would be lying in precisely the way §1.4 named.

**Date** 2026-08-20 · **Lane** H1 · **Base** `26e4b5db4` on
`claude/jdk-only-mode-handoff-09b48c`
**Subject** `--jdk-only-report`'s observation sinks; `regression-suite/run.sh`;
the P0 row *Native-first dispatch and hard-coded overrides*
**Answers** `G90-1` §8 N1, `G84-1` §4 N3, `HANDOFF-20260819` §7 item 3

---

## 0. The one-line summary

Two of the three items landed as code. **The third was already done, in
2026-08-04, and the P0 row still describes the tree as it was before** — with a
*"Provenance: re-verified"* stamp on the paragraph that is wrong, and naming a
file the symbol has never lived in. That is the fourth stale P0 row this project
has found by reading the tree instead of the table, and the fifth this week.

---

## 1. H1-A — the observation sink capped every shadow count and only a boolean said so

### 1.1 What was already there, and why that matters more than what was missing

`README.md` Rule 1 first. **Half of the item's stated remedy had already
landed** and the task brief did not know it:

| Asked for | State before this change |
|---|---|
| an env-settable cap | **already present** — `CRATONVM_NATIVE_SHADOW_SINK_CAP`, declared in `types/src/flag_groups.rs:456` as `CRATONVM_DBG=native-shadow-sink-cap`, read through `jdk_only_native_shadow_cap()` with a 65,536 ceiling |
| an explicit `truncated` signal | a `saturated` boolean in `observation_sink`, plus a warning line in `vm-cli` |
| a dropped-observation counter | **absent** |
| the same for the OTHER bounded sinks | **absent — and no signal of any kind** |

So the honest framing of `G90-1` N1 is not "nothing was done"; it is **"the
identities were bounded and the magnitude was thrown away"**. A boolean tells a
reader the list is short. It cannot tell them by how much, and there is no
arithmetic that recovers the difference from the file. That is why 980 → 943 is
a difference of two censored measurements.

### 1.2 The defect underneath the defect

The cap was not the only truncation. `jdk_only_shadow_already_observed`
(`vm/src/vm/vm_exec.rs`) opened with:

```rust
if JDK_ONLY_NATIVE_SHADOW_FULL.load(Ordering::Relaxed) {
    return true;
}
```

The caller (`native_override.rs :: …` around the `let ask = strict_bridge && …`
term) uses that answer to decide whether to pay for the class-manager read lock
and hierarchy walk that *discovers* a shadow. So once the sink filled, every
triple answered "already seen", the walk stopped, and
`refusals.interpreter_shadow_unenforced` **froze** — the file's own doc said so
and treated it as a property rather than a bug.

**A drop counter alone would therefore have counted nothing.** The producer was
gated on the same flag as the consumer. Adding `dropped` without removing that
short-circuit would have shipped a counter that reads `1` on a saturated run and
looks like good news — the exact shape this directory keeps filing.

### 1.3 What changed

**`vm/src/vm/vm_exec.rs`**

* `JDK_ONLY_NATIVE_SHADOW_CAP` **256 → 4096**, with the arithmetic in the doc.
* `JDK_ONLY_NATIVE_SHADOW_DROPPED: AtomicU64`, and
  `jdk_only_native_shadow_sink_dropped()`.
* `offer_native_shadow_observation` now consults the filter **before** the full
  flag, counts a drop, and **publishes the dropped triple's digest to the
  filter**. Publishing is the part that keeps this cheap.
* `jdk_only_shadow_already_observed` no longer short-circuits on the full flag;
  it answers from the filter in both states.
* The docs on `jdk_only_native_shadow_unenforced`,
  `…_sink_saturated` and `jdk_only_native_shadow_cap` are corrected where they
  now describe behaviour that no longer exists.

**`vm/src/jit/helpers.rs`** — this sink had **no saturation signal at all**,
which nobody had noticed because the report only ever described the interpreter's:

* `JDK_ONLY_HELPER_VIOLATION_CAP` 256 → 4096, `…_CAP_MAX = 65_536`,
  `jdk_only_jit_helper_violation_cap()` reading the **same declared env var**.
* `JDK_ONLY_HELPER_VIOLATIONS_DROPPED`, plus `…_sink_len()`, `…_sink_dropped()`,
  `…_sink_saturated()`.
* `record_jdk_only_fastpath_refusal` tests dedup **before** capacity. The old
  `len() < CAP && !contains` could not tell "this is a repeat" from "there was
  no room", which is why a drop could not be counted at all.

**`vm/src/vm/vm_init.rs`** (the report serialiser) — `observation_sink` gains
`truncated`, `dropped`, and two per-source sub-objects:

```json
"observation_sink": {
  "recorded": 81, "cap": 4096, "saturated": false,
  "truncated": false, "dropped": 0,
  "jit_fastpath": { "recorded": 3, "cap": 4096, "truncated": false, "dropped": 0 },
  "jit_compile":  { "recorded": 0, "cap": 256, "truncated": null, "dropped": null }
}
```

**`vm-cli/src/main.rs`** — the human-readable warning now prints the drop count,
the implied total, and the `CRATONVM_NATIVE_SHADOW_SINK_CAP` value to re-run
with. It used to end with *"Narrow the workload to read the list as
exhaustive"*, which is the advice that manufactured the floors.

### 1.4 Why `jit_compile` is `null` and not `false`

`cratonvm_jit::record_jdk_only_direct_native_refusal` (`jit/src/lib.rs`) is the
third bounded collection feeding `violations[]`. It is capped at 256 and has no
counter. **This lane does not own that file**, so the report says `null` — an
unmeasured thing must not render as a clean one. The exact patch is in §5.

### 1.5 Determinism — what I did and did not do

**The retained SET is unchanged.** Saturation is still decided at the same
instant by the same offer, and nothing is pushed after the flag is set. What
changed is only what happens to offers that arrive *after* that point.

**It was not deterministic before, and it still is not, and that is worth
stating rather than papering over.** The sink is filled from arbitrary mutator
threads, so "the first 4096 distinct arrivals" can differ between two runs of
the same vector. The report SORTS its output, so the file is byte-stable given
the same set — but the set itself is arrival-ordered. Making retention
deterministic needs an ordering key (class/method/descriptor) and a
priority-queue-shaped sink; that is a real change, it is nominated in §6, and I
did not do it. **What this change does do is make the nondeterminism
observable**: two runs that retain different sets now report the same
`recorded + dropped`, and a reader can see they disagree only on which rows they
had room to name.

### 1.6 What H1-A does NOT do

* It does **not** make `dropped` an exact distinct-triple count. The
  interpreter's is filter-mediated (512 slots, deliberately not resized with the
  cap), so once the dropped population exceeds the filter, a collision can count
  one triple twice. The JIT helper's is a plain event count, because that sink
  has no filter to survive the drop. **Both err upward**, and both docs say so at
  the accessor. The way to get an exact list is to raise the cap until `dropped`
  reads zero, which now needs no rebuild.
* It does **not** instrument `cratonvm_jit`'s compile-time sink (§5).
* It does **not** re-measure anything. There is no build.
* **PREDICTED:** no `--jdk-only` regression vector saturates at 4096, so the
  36-vector union becomes a total rather than a floor. **Falsified by** any
  report with `observation_sink.truncated: true`, which is now printed by
  `run.sh` (§3) and does not require anyone to open the JSON.
* **PREDICTED:** removing the short-circuit costs nothing on an unsaturated run
  (identical code path) and, on a saturated one, one hierarchy walk per newly
  discovered distinct triple. **Falsified by** a `--jdk-only` arm that slows
  measurably or times out where it did not; the escape is to raise the cap so
  saturation never occurs. If it *does* regress, the filter width
  (`JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS`, 512) is the thing to look at first.

### 1.7 A doc argument I deleted, because it is wrong

The doc on `jdk_only_native_shadow_cap()` argued against raising the default:
*"A default nobody asked for that costs every strict run more memory is a change
to the shipping configuration."* **That does not survive reading `Vec::push`.**
The sink's memory is proportional to the observations it actually holds, not to
its ceiling. A run with 80 shadows allocates 80 rows whether the cap is 256 or
4096. The argument was load-bearing — it is why the number stayed at 256 while
three vectors overflowed it — and it was never true.

---

## 2. H1-B — the row expired on 2026-08-04; here is the grep

### 2.1 The row says

> `real_protected_stub_class` (`vm/src/runtime/interpreter/invoke.rs`) is a
> hand-maintained class allowlist … **Correction: that is one of two copies, and
> they are not identical.** The copy inside `invoke_or_native`
> (`vm/src/vm/vm_exec.rs`) lists the same ten **plus `java/util/StringJoiner`**
> (11); `real_protected_stub_class` omits it deliberately … *Provenance:
> re-verified — both lists re-read in `vm/src/vm/vm_exec.rs` (11 classes, `COPY
> 1 OF 2` marker present in the tree)*

`docs/jdk-only-runtime-services.md:86`.

### 2.2 The tree says

```
$ grep -rn "COPY 1 OF 2\|COPY 2 OF 2" --include=*.rs .
(no output)

$ grep -rn "fn real_protected_stub_class" --include=*.rs .
vm/src/runtime/interpreter/native_override.rs:6892:pub(crate) fn real_protected_stub_class
vm/src/runtime/interpreter/native_override.rs:6903:fn real_protected_stub_class_common

$ grep -c "real_protected_stub_class(" vm/src/vm/vm_exec.rs
1

$ git log -S'real_protected_stub_class' --oneline | head
…
7e45c417d fix(jdk-only): retire the real-protected-stub path divergence
b90f9a614 fix(jdk-only): items 3+8 — one list per policy, and revive a dead h2-bnf fix
```

**Four separate ways the row is wrong about the tree:**

1. **There is one list, not two.** `real_protected_stub_class_common`, a single
   `matches!`, read by both dispatch paths. `vm_exec.rs` calls the predicate; it
   holds no literals. The `COPY 1 OF 2` marker the row says is "present in the
   tree" is **not in the tree**. It did exist: `git log -S'COPY 1 OF 2'` names
   four commits, the newest being `b90f9a614 fix(jdk-only): items 3+8 — one list
   per policy`, which is the commit that removed it. The row's provenance note
   was true when written and was re-stamped without being re-run.
2. **It has TWELVE entries, not 11 or 10** — `ThreadPoolExecutor` was added
   2026-08-06, after the row was written.
3. **`java/util/StringJoiner` is protected on BOTH paths**, since 2026-08-04.
   The HIB-CV-32 heap-integrity trip was re-measured with the merge applied
   (40k `add()` calls under `-Xmx64m`, seven intermediate consistency checks,
   against a HotSpot control) and **did not reproduce** in either mode. The
   21-line comment the row cites still exists — as the *history* of a retired
   exception, which is exactly the shape that makes a stale row look verified.
4. **The symbol is not in the file the row names.**
   `vm/src/runtime/interpreter/invoke.rs` does not define it and, as far as
   `git log -S` shows, never did. A reader following the row goes to `invoke.rs`,
   finds no list, and the most natural conclusion — "so it was already removed" —
   is right for the wrong reason.

`HANDOFF-20260819.md` §1 records this staleness against the **Duplicate dispatch
implementations** row (`G86-1`, `G87-1`), and that row was annotated. **The
*Native-first dispatch* row was not**, although it carries the same paragraph
and the same `StringJoiner` claim. Two rows, one correction, applied to one.

### 2.3 So I did not do what the item asked, and here is why

The item asked for a table of `(class, cold_path, warm_path, reason)` with
`StringJoiner` as the single `warm-only` row. **Building that would recreate the
divergence.** There is no per-path column any more; every class answers the same
on both paths. A two-column table whose columns are provably identical is a
guard that cannot fail — the exact criticism `native_override.rs`'s own test
comment makes of the assertion it replaced:

> *"There is now a single predicate, so there is nothing left to compare the
> paths against — an 'the paths agree' assertion over one function would be a
> guard that cannot fail."*

### 2.4 What I did instead

The membership ratchet the item asks for **already exists**:
`native_override.rs :: every_allowlisted_class_is_protected`, over
`REAL_PROTECTED_STUB_CORPUS` (12 entries, floor of 12, with a message explaining
what removing `StringJoiner` costs). That file is not mine and it needs nothing.

What did **not** exist is a check on the property that is actually still at
risk — that nobody re-inlines a copy. Today the only thing preventing it is a
comment at the call site saying *"Do NOT re-inline a copy here"*, and **a premise
in a comment is not a compile-time link**. So `vm/src/vm/vm_exec.rs` gains
`#[cfg(test)] mod real_protected_stub_single_predicate_witness`, three tests,
all source-witness against the working tree:

* `the_cold_path_re_inlines_no_copy_of_the_allowlist` — ten of the twelve
  class-name literals, **quoted**, must not appear in `vm_exec.rs`;
* `the_cold_path_asks_the_one_predicate_exactly_once` — one call site, frozen;
* `exactly_one_definition_of_the_predicate_exists` — one
  `real_protected_stub_class_common`, and `invoke.rs` still does not define one.

The module cuts itself out of the text it scans, so it cannot be satisfied by its
own literals.

### 2.5 A third override surface the row does not mention

Writing that test found it. Two of the twelve names —
`"java/lang/management/ManagementFactory"` and
`"java/util/concurrent/ThreadPoolExecutor"` — **do** appear as quoted literals in
`vm/src/vm/vm_exec.rs`, inside `invoke_on_class_shared_inner`
(around lines 24655 and 25201). They are not a second copy of the allow-list:
they belong to a **separate, method-scoped table with the opposite polarity** —
force the native to win *over* real bytecode — which also carries
`java/util/logging/Logger`, `java/util/logging/LogRecord`,
`org/apache/juli/AsyncFileHandler$LoggerExecutorService`,
`org/apache/juli/FileHandler` and `org/jboss/logmanager/Logger`.

The P0 row is titled *"Native-first dispatch and **hard-coded overrides**"* and
this is a hard-coded override table with app-specific entries in it. **It is not
in the row's inventory of five predicates.** Nominated in §6; not touched here,
because changing it changes behaviour and this item was explicitly
behaviour-preserving.

### 2.6 What H1-B does NOT do

* No behaviour changed for any class. No entry added, removed or re-scoped.
* It does **not** correct `docs/jdk-only-runtime-services.md:86` — that file is
  orchestrator-owned. §5 carries the exact replacement text.
* It does **not** close the row. The row is wider than the allow-list: it also
  covers `invoke_or_native` consulting the registry before ordinary dispatch,
  and the third table in §2.5.

---

## 3. H1-C — the strict report is now part of the arm

`regression-suite/run.sh`. When `CRATONVM_ARGS` names `--jdk-only`, every vector
is additionally given `--jdk-only-report` into a **PID-scoped** directory
(`regression-suite/.jdk-only-reports.$$`), and the run ends with a census block
after `COUNTS:`.

```
JDK-ONLY CENSUS (102 of 102 per-vector reports written):
  native-shadows-bytecode, UNION over vectors: N native-won (the defect), M bytecode-won (the contract working)
  synthetic-native-registered, UNION: …   ·   interpreter_shadow_unenforced, SUM: …   ·   compatibility_classes, SUM: …
  saturation: none — no report truncated a bounded collection, so the counts above are totals, not floors.
```

**The union is a real union, not a sum.** `types::error::to_json()` has no
pretty-printer, so every violation row is ONE LINE of compact JSON and `summary`
is a pure function of the other fields — identical facts from two vectors are
byte-identical lines, and `sort -u` dedups them exactly. No `jq` required.
Counters (`interpreter_shadow_unenforced`, `compatibility_classes`) are SUMMED,
because they are per-process event counts and summing independent runs is the
right operation; the summary labels which is which.

The constraints, and how each is met:

* **Cannot change a verdict.** `write_jdk_only_dumps` returns `()`; a write
  failure is an `eprintln!`, not an error. Its lines begin `[cratonvm] `, and
  `extract()` (`harness-guard.sh:119`) keeps only `^(PASS|CK) `, so nothing
  reaches the cross-VM diff. None of its text matches the crash grep
  (`SIGSEGV|rust panic|fatal runtime error|stack overflow`). HotSpot's command
  line is untouched.
* **No second fixed shared path.** PID-scoped, `rm -rf`'d at the end, and added
  to `regression-suite/.gitignore` so a killed run cannot feed this checkout's
  auto-commit sweep a few hundred JSON files.
* **Per-vector launch args undisturbed.** The flag is passed as a bash **array**,
  not appended to `$cvextra`/`$extra` — those are expanded unquoted on purpose
  and a path that may contain spaces cannot join them.
* **A missing report is visible.** `report_expected` is incremented at the launch
  site, not derived from `$CLASSES`, and the summary prints the shortfall.
* Suppressed entirely if the operator passed their own `--jdk-only-report`.

`bash -n regression-suite/run.sh` passes. That is a parse check, not a run.

### 3.1 What H1-C does NOT do

* **It has never been executed.** No arm has been run. The awk-free grep/sed
  pipeline is argued from the JSON shape in `types/src/error.rs`, not from a
  file it produced. **Falsified by** a census block printing zeros on a run whose
  reports contain rows.
* It adds no baseline and no gate. The census cannot turn a run red, on purpose
  (`G84-1` N3: unlike the stub ratchet, this is informative without a frozen
  number, and a frozen number is what makes the ratchet unable to tell a
  regression from an improvement).
* It does not split the union by prefix. `G90-1` N3's next sweep wants
  per-package counts; this prints one number.

---

## 4. Files touched

| File | Item |
|---|---|
| `vm/src/vm/vm_exec.rs` | H1-A (cap, drop counter, filter/full reordering, saturation short-circuit removed) + H1-B (witness test module) |
| `vm/src/jit/helpers.rs` | H1-A (second sink: cap, override, drop counter, accessors) |
| `vm/src/vm/vm_init.rs` | H1-A (report serialiser: `truncated`, `dropped`, two sub-objects) |
| `vm-cli/src/main.rs` | H1-A (human-readable warnings for both sinks) |
| `regression-suite/run.sh` | H1-C |
| `regression-suite/.gitignore` | H1-C (PID-scoped report dir) |
| this record | — |

`vm/src/runtime/interpreter/invoke.rs` is owned by this lane and **was not
edited** — it turned out to contain none of the code the row attributes to it.

---

## 5. OUT-OF-FILE EDITS REQUIRED

### 5.1 `jit/src/lib.rs` — the third sink still has no drop counter

`jit/src/lib.rs:8632` currently:

```rust
/// Maximum number of distinct structured violations the JIT retains.
pub const JDK_ONLY_VIOLATION_CAP: usize = 256;
```

Add after it:

```rust
/// Distinct violations this sink had no room for. See
/// `cratonvm_vm::vm::jdk_only_native_shadow_sink_dropped` for why a boolean is
/// not enough: the report renders this sink's `truncated`/`dropped` as `null`
/// until it exists, because an unmeasured thing must not render as a clean one.
static JDK_ONLY_VIOLATIONS_DROPPED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many violations this sink had no room for. An EVENT count: dedup here is
/// `Vec::contains` over RETAINED rows, so a repeat of a dropped row counts
/// again. It errs upward, which is the safe direction for a floor warning.
pub fn jdk_only_jit_sink_dropped() -> u64 {
    JDK_ONLY_VIOLATIONS_DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}
```

and in `record_jdk_only_direct_native_refusal` (`jit/src/lib.rs:8654`) replace:

```rust
    let mut recorded = jdk_only_violations().lock();
    if recorded.len() >= JDK_ONLY_VIOLATION_CAP {
        return;
    }
```

…and its trailing…

```rust
    if !recorded.contains(&violation) {
        recorded.push(violation);
    }
}
```

…with a single dedup-first block (the violation must be built before the
capacity test, exactly as `helpers.rs` now does):

```rust
    let mut recorded = jdk_only_violations().lock();
    if !recorded.contains(&violation) {
        if recorded.len() < JDK_ONLY_VIOLATION_CAP {
            recorded.push(violation);
        } else {
            JDK_ONLY_VIOLATIONS_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
```

Then in `vm/src/vm/vm_init.rs`, replace the two literal `null`s in the
`jit_compile` object with `cratonvm_jit::jdk_only_jit_sink_dropped() > 0` and
`cratonvm_jit::jdk_only_jit_sink_dropped()`. **Raising `JDK_ONLY_VIOLATION_CAP`
to 4096 for symmetry is optional and is a separate decision** — that sink is
filled at compile time, not per dispatch, and nothing has been measured to
overflow it.

### 5.2 `docs/jdk-only-runtime-services.md:86` — the *Native-first dispatch* row

Current text, from *"**Correction: that is one of two copies…**"* through
*"…`jdk-only-real-protected-stub-allowlists-FIXED-20260804.md`).*"*, should be
replaced by:

> **CORRECTION (H1-1, 2026-08-20, verified by grep): the "two copies" premise
> expired on 2026-08-04 and this row was not annotated when its twin was.** There
> is ONE predicate — `native_override.rs :: real_protected_stub_class_common`,
> **twelve** entries, read by both dispatch paths — not two copies of ten and
> eleven. `java/util/StringJoiner` is protected on **both** paths; the HIB-CV-32
> heap-integrity trip was re-measured with the merge applied and did not
> reproduce, and the 21-line comment this row cites is the retired exception's
> history, not a live divergence. `java/util/concurrent/ThreadPoolExecutor` was
> added 2026-08-06, which is why the count is twelve. **`COPY 1 OF 2` does not
> exist in the tree** (`grep -rn "COPY 1 OF 2" --include=*.rs .` is empty;
> `git log -S` shows `b90f9a614` removed it), and
> `real_protected_stub_class` is **not** in
> `vm/src/runtime/interpreter/invoke.rs` — it is in `native_override.rs:6892`.
> The *"Provenance: re-verified"* stamp on the deleted text was wrong on all
> four points. See `known-issues/jdk-only/H1-1-…-20260820.md` §2 and `G86-1`.
> Ratchets: `every_allowlisted_class_is_protected` (membership, floor of 12) and
> `real_protected_stub_single_predicate_witness` (`vm/src/vm/vm_exec.rs`, three
> source-witness tests forbidding a re-inlined copy). **What remains open in this
> row** is (a) `invoke_or_native` consulting the registry before ordinary
> bytecode dispatch, and (b) a THIRD hard-coded override table this row never
> inventoried — see §2.5 of the H1-1 record.

Its *Required resolution* cell still says *"**Reconcile the two lists, do not
merge them**"*. That instruction is now unexecutable and should be struck; the
lists were reconciled two weeks ago and the sentence reads as pending work.

### 5.3 `docs/known-issues/jdk-only/HANDOFF-20260819.md` §4

The instrument table's `--jdk-only-report` row reads *"the sink caps at 256 and
only a boolean says so — every count is a floor"*. After this change: *"the sinks
cap at 4096 (`CRATONVM_NATIVE_SHADOW_SINK_CAP`) and report `truncated` +
`dropped` per collection; a count is a floor only when `dropped > 0`, and
`run.sh` prints how many reports truncated."*

### 5.4 `docs/known-issues/jdk-only/INDEX.md`

No row for this record. Orchestrator-owned.

---

## 6. NOMINATIONS

* **N1 — resize the shadow filter with the cap.**
  `JDK_ONLY_NATIVE_SHADOW_FILTER_SLOTS` is a fixed 512-entry static array, so it
  cannot follow `CRATONVM_NATIVE_SHADOW_SINK_CAP`. Above ~512 distinct triples
  the filter stops absorbing repeats, which costs one redundant hierarchy walk
  per eviction and makes `dropped` less exact. A `OnceLock<Box<[AtomicU64]>>`
  sized at first use would fix both. Cheap; not done because it is a second
  allocation on a path this lane could not measure.

* **N2 — make the retained SET deterministic.** §1.5. Two runs of one vector can
  retain different rows because retention is arrival-ordered across mutator
  threads. A sink keyed on `(class, method, descriptor)` that evicts the
  lexicographically-largest when full would make the retained set a function of
  the observed set. This is the only way `violations[]` becomes diffable between
  two commits, which is what `G89-1` §2's two-column rule wants for shadows.

* **N3 — inventory the THIRD override table.** §2.5.
  `invoke_on_class_shared_inner` in `vm/src/vm/vm_exec.rs` carries a large
  method-scoped force-native table including `org/apache/juli/*` and
  `org/jboss/logmanager/Logger` — application class names hard-coded into the
  VM's dispatch path. The P0 row's five-predicate inventory does not include it.
  Under `--jdk-only` its polarity is precisely what §1.4 forbids, so it deserves
  the same `outcome`-split census the allow-list got. **Start by asking whether
  any of it fires under strict mode at all** — `G86-1` found the allow-list was
  already dead there, and this could be too.

* **N4 — the `dropped` counters have no test.** Everything in §1 is argued from
  reading. A unit test that offers `cap + 3` distinct triples and asserts
  `recorded == cap && dropped == 3` would catch a future reordering of the
  filter/capacity tests, which is exactly the mistake the old
  `len() < CAP && !contains` was. I did not add one because this lane is
  forbidden to build, and an untested test is worse than none.

* **N5 — split the run.sh census by package prefix.** `G90-1` N3's next sweep
  needs per-prefix counts to choose which prefix to arm. The union pipeline
  already has the class name in each row; one more `sed`/`sort | uniq -c` turns
  the single number into the table that record had to build by hand.

---

## 7. Where this record disagrees with the existing ones

Collected, because per `README.md` this is the highest-value output:

1. **`docs/jdk-only-runtime-services.md:86` is stale on four separate points**
   and carries a *"re-verified"* provenance stamp. §2.2. Its twin row was
   corrected on 2026-08-19 and this one was not.
2. **`G90-1` N1 and `HANDOFF` §7.3 describe the env override as missing.** It
   has existed since `G60-1`. §1.1. The missing half was the counter, and saying
   "raise the cap" as the remedy would have produced a no-op commit.
3. **`HANDOFF` §4 says the sink "caps at 256"** — singular. There are **three**
   bounded collections feeding `violations[]`, at 256 each, and two of them had
   no saturation signal in the file at all. A reader checking `saturated: false`
   was reading about one source of three.
4. **`vm_exec.rs`'s own doc argued that raising the default costs every strict
   run memory.** It does not; the `Vec` grows by `push`. §1.7. That argument is
   why the constant stayed at 256 while three vectors overflowed it.
5. **`jdk_only_native_shadow_unenforced`'s doc described its own truncation as a
   design tradeoff** (*"the magnitude only has to be non-zero"*). The magnitude
   was the thing three P0 rows were arguing about.
