# Phase accounting

Where a CratonVM run's wall clock actually went, as named categories plus an
explicit unattributed remainder.

**Why this exists.** The C2 review
has a P0 lane *"Separate startup, compilation, execution, and GC time"*, whose
acceptance criterion is:

> Every benchmark's wall time reconciles to named categories within 2%.

Nothing in the tree could answer that. `jit/src/metrics.rs` measures the inside
of one compilation. `gc/src/gc_metrics.rs` counts cards and names the collector
decision. `regression-suite/perf/` measures the wall clock. None of them
connects the three, and none of them can say what fraction of a run is
*unaccounted for* — which is the number the criterion is actually about.

Everything below is produced by `cratonvm_jfr::phase` (`jfr/src/phase.rs`).

---

## Contents

1. [The category model](#1-the-category-model)
2. [The reconciliation identity](#2-the-reconciliation-identity)
3. [Why double-counting is structurally impossible](#3-why-double-counting-is-structurally-impossible)
4. [Levels, and the interpreted-vs-compiled split](#4-levels-and-the-interpreted-vs-compiled-split)
5. [Enabling it](#5-enabling-it)
6. [The JSON schema](#6-the-json-schema)
7. [The stderr summary line](#7-the-stderr-summary-line)
8. [The JFR sink](#8-the-jfr-sink)
9. [Verifying the 2% criterion on a real run](#9-verifying-the-2-criterion-on-a-real-run)
10. [Wiring plan](#10-wiring-plan)
11. [What remains unattributable, and why](#11-what-remains-unattributable-and-why)

---

## 1. The category model

Sixteen categories, closed set. The names are an external contract: they key
the JSON, the JFR payloads and the stderr line, and a consumer may depend on
them.

| Category | What it covers |
|---|---|
| `vm_startup` | `Vm::new`: heap reservation, bootstrap class loading, native registration, module graph — everything before the application's `main` is entered |
| `class_load` | reading, parsing, defining, linking a class, and running its `<clinit>` |
| `verification` | bytecode verification (`classloading::verifier`). Split out of `class_load` because it is the part an operator can turn off |
| `interpretation` | interpreting bytecode. **`fine` level only** |
| `jit_execution` | executing JIT-compiled code. **`fine` level only** |
| `java_execution` | interpreted and compiled execution, undivided. **`coarse` level only** |
| `compile_queue_wait` | a mutator blocked waiting for a compilation it requested |
| `compilation` | running the compiler |
| `code_install` | publishing a compiled body: code-cache allocation, relocation, entry patching, inline-cache and vtable updates |
| `deoptimization` | frame reconstruction and the transfer back to the interpreter |
| `gc_pause` | stop-the-world collection pauses, including mutator time blocked in them |
| `gc_concurrent` | concurrent collector work running alongside mutators |
| `safepoint` | at a safepoint for something other than a GC (deopt storms, redefinition, stack walks, jcmd) |
| `native_call` | inside a native method or a foreign downcall — the transition and the callee both |
| `idle` | parked with no work: the background compiler on its condvar, a GC worker between cycles, a pooled thread between tasks |
| `vm_shutdown` | teardown after `main` returns: shutdown hooks, finalization, report emission |

Plus three derived quantities that are **not** categories:

| Quantity | Meaning |
|---|---|
| `open_ns` | elapsed time inside spans that were still open when the report was taken. Neither charged nor unattributed — in flight |
| `unattributed_ns` | `wall - attributed - open`. The measurement the 2% criterion is about |
| `over_attributed_ns` | `attributed + open - wall` when positive. Always `0` on correct wiring; non-zero is a defect, never a workload property |

### Accounting is per thread

Each thread's timeline is partitioned independently. This matters:

* The **reconciliation basis** is one thread — the one that called
  `mark_process_start()`, i.e. the launcher thread. Its wall clock is what a
  benchmark's wall time means. `basis_wall_ns` and `residual_ns` are its
  numbers, and `reconciles` is its verdict.
* `totals` (per category, summed over threads) is **thread time, not wall
  time**. On a multi-threaded run it legitimately exceeds `process_wall_ns`.
  Comparing it against the wall clock is a category error; the report keeps
  `thread_wall_ns` alongside it so the comparison that *is* meaningful is
  available.
* The background compiler's idle time is `idle` on the compiler's own thread.
  It is not mutator wall time, and it does not enter the basis thread's
  accounting at all. `compile_queue_wait` is the mutator-side figure and is
  zero on a run whose compilation is entirely asynchronous.

### Compilation is delegated, not re-timed

`compilation` is a single span around the compiler's entry point. This module
does **not** re-time `scan` / `build` / `optimize` / `escape_analysis` /
`verify` / `schedule` / `lower` / `single_pass` — `jit::metrics::Phase` already
does, and a second set of timers around the same code would disagree with the
first and there would be no way to say which was right.

Instead the shutdown path pushes `jit::metrics::summary().phase_totals_ns` in
through `phase::set_compilation_breakdown`, and the report carries it as
`compilation_breakdown` — a breakdown *of* the `compilation` bucket, never
added to it — together with `compilation_delta_ns`, the signed disagreement
between the two measurements. A non-zero delta is expected and informative:
the span brackets queue handoff and publication that `jit::metrics` does not
time, and `jit::metrics`' bounded ring may have evicted reports the span still
counted.

This also keeps `cratonvm-jfr` free of a `cratonvm-jit` dependency.

---

## 2. The reconciliation identity

For every thread, **by construction rather than by arithmetic afterwards**:

```
thread_wall_ns = Σ category_self_ns + open_ns + unattributed_ns
```

`unattributed_ns` is a *measurement*. A run where 30% of the clock is in no
category is a run this module reports as 30% unattributed. It is never folded
into `java_execution`, never clamped, and never explained away.

The mirror-image failure is equally visible. If the categories ever sum past
the wall clock, that surplus appears as `over_attributed_ns` rather than
shrinking `unattributed_ns` below zero. A report with a non-zero
`over_attributed_ns` is reporting a wiring bug, and the tests assert it stays
zero.

`jfr/src/phase.rs`'s test
`categories_plus_unattributed_sum_to_the_measured_wall_time` asserts the
identity directly; `a_deliberate_gap_lands_in_unattributed_not_in_a_category`
asserts that an uninstrumented 2 ms hole shows up in `unattributed_ns` and
makes `reconciles()` false.

---

## 3. Why double-counting is structurally impossible

Phases nest. A class load happens inside interpretation; a GC happens inside a
class load; a deoptimization happens inside compiled execution. Charging each
span its full elapsed time would count the same nanoseconds two or three times
and the totals would exceed the wall clock without anything actually being
wrong.

**Self-time accounting.** Each thread keeps a stack of open spans. Each frame
carries a `child_ns` accumulator. When a span closes:

1. its whole elapsed time is added to its **parent's** `child_ns`;
2. `elapsed - child_ns` — its *self* time — is charged to its own category.

Therefore, at any instant, a nanosecond on a thread is charged to exactly one
category (the innermost span open at that instant) or to nothing at all, in
which case it lands in `unattributed_ns`. There is no code path that adds a
duration to two categories, because there is no code path that adds a duration
to anything other than the frame currently being popped.

**The stack discipline is enforced, not assumed.** Each frame carries a token;
`PhaseSpan::drop` locates its own frame by token rather than assuming it is on
top. Three cases:

| Case | Behaviour |
|---|---|
| Normal LIFO drop | the top frame is this span's; charge and pop |
| Out-of-order drop (a span dropped while descendants are still open) | close every frame above it too — their time is still charged exactly once, to the right categories — and increment `anomalies.out_of_order_closes` so the report says the wiring is wrong |
| Span never dropped (leaked, or thread killed) | charges nothing; its time appears as unattributed. Visible, not absorbed |

`out_of_order_close_is_counted_and_still_charges_once` covers the second case:
it asserts the anomaly counter is `1`, that the inner span's entry count is
still exactly `1` after its late drop, and that `over_attributed_ns` is `0`.

**Concurrency.** Per-thread state is thread-local and lock-free on the hot
path; only the owning thread writes its counters (relaxed atomic adds), and the
reporter reads them. `concurrent_threads_neither_lose_nor_double_count` runs
eight threads × 25 nested span pairs and asserts the entry counts are exactly
200 apiece, no ordering anomalies, and that every worker's parts sum to its own
wall clock.

---

## 4. Levels, and the interpreted-vs-compiled split

Splitting interpreted from compiled execution needs a span at every method
entry — exactly the kind of instrumentation that perturbs the number it
reports. So there are two levels:

| Level | Executor spans | Cost |
|---|---|---|
| `coarse` | one `java_execution` span around the whole Java run | negligible |
| `fine` | `interpretation` and `jit_execution` per method entry | two clock reads per method entry |

The two are mutually exclusive by construction (`Category::active_at`): at
`coarse`, opening an `interpretation` span records nothing; at `fine`, opening
a `java_execution` span records nothing. A call site may therefore open all
three unconditionally and let the level decide.

At `fine`, dispatch overhead *between* an interpretation span and a JIT span
becomes unattributed. That is honest, and it is why the two levels are reported
separately rather than blended: a `fine` run's `unattributed_ns` includes a
per-call component that a `coarse` run's does not.

**Use `coarse` for the 2% gate.** `fine` answers "how much of execution is
interpreted", which is a different question and is not expected to reconcile as
tightly.

---

## 5. Enabling it

| Flag | Default | Meaning |
|---|---|---|
| `CRATONVM_PHASE_ACCOUNTING` | off | `1`/`true`/`yes`/`on`/`coarse` → `coarse`; `2`/`fine` → `fine`; anything else off |
| `CRATONVM_PHASE_ACCOUNTING_OUT` | unset | path for the JSON report, written once at shutdown |
| `CRATONVM_PHASE_ACCOUNTING_JFR` | unset | path for a standalone JFR chunk |

Read through `cratonvm_types::flags::runtime_var_os`, matching
`jit/src/metrics.rs`. The level is latched on first read.

Because they are `Group::DBG` tokens they are also reachable as
`CRATONVM_DBG=phase-accounting=coarse,phase-accounting-out=/tmp/p.json`, which
is the form that survives `-XX:` expansion and `with_thread_overrides`.

> These three names are declared in `types/src/flag_groups.rs:425-427` and
> `types/tests/flag-surface.txt:541-543`. That declaration is load-bearing:
> `types/tests/flag_declaration_guard.rs` fails the workspace test suite on any
> exact `"CRATONVM_*"` string literal in a Rust source that is not in
> `flag_groups::INVENTORY`/`SCALARS`, and `jfr/src/phase.rs` contains three.
> See [§10.1](#101-done-the-three-flags-are-declared).

**Cost when disabled.** `phase::enter` performs one relaxed atomic load and
returns a span whose token is `None`: no allocation, no clock read, no
thread-local touch, no registry lock. Its `Drop` is an `Option` test that
returns. `disabled_records_nothing` asserts that a disabled run does not even
register a thread account.

**Memory bound.** At most `MAX_TRACKED_THREADS` (4096) thread accounts, each a
fixed-size struct of 36 `u64`s plus a name. Threads past the cap record nothing
and are counted in `anomalies.threads_dropped`, so their absence is visible
rather than silently shrinking the totals.

---

## 6. The JSON schema

One document (not JSON-lines — a phase report is a whole-run artefact and the
harness wants to `json.load` it). `schema_version` is `1`; it is bumped when a
key is removed or changes meaning, not when one is added, so consumers must
ignore unknown keys.

```json
{
  "schema_version": 1,
  "level": "coarse",
  "process_wall_ns": 4127883100,
  "tolerance": 0.02,
  "reconciles": true,
  "residual_ppm": 3114,
  "basis_wall_ns": 4127801200,
  "residual_ns": 12854000,
  "thread_wall_ns": 9330112400,
  "attributed_ns": 9317258400,
  "open_ns": 0,
  "unattributed_ns": 12854000,
  "over_attributed_ns": 0,
  "totals":  { "vm_startup": 311208400, "class_load": 88110300, "…": 0 },
  "entries": { "vm_startup": 1,         "class_load": 1842,     "…": 0 },
  "primary": { "…one thread object…" },
  "threads": [ { "…thread object…" } ],
  "compilation_breakdown": { "scan": 411200, "build": 1880400, "…": 0 },
  "compilation_delta_ns": -204100,
  "anomalies": {
    "out_of_order_closes": 0,
    "open_spans": 0,
    "threads_dropped": 0
  }
}
```

A thread object:

```json
{
  "thread_id": 1,
  "name": "main",
  "primary": true,
  "start_ns": 0,
  "end_ns": null,
  "wall_ns": 4127801200,
  "attributed_ns": 4114947200,
  "open_ns": 0,
  "unattributed_ns": 12854000,
  "over_attributed_ns": 0,
  "open_spans": 0,
  "out_of_order_closes": 0,
  "categories": { "vm_startup": 311208400, "…": 0 },
  "entries":    { "vm_startup": 1,         "…": 0 }
}
```

Notes a consumer needs:

* **Every category key is always present**, at `0` when nothing recorded it, in
  `Category::ALL` order. Two runs therefore diff cleanly, and a missing key
  means a schema change rather than a quiet zero.
* `end_ns` is `null` while a thread is still running (the basis thread always
  is, since the report is taken from it).
* All durations are nanoseconds, integers. `tolerance` is the only float.
* `residual_ppm` is parts per million so that a text-scraping consumer never
  has to parse a decimal point.
* `compilation_breakdown` keys are `jit::metrics::Phase` names, verbatim.
  Absent (`{}`) when nothing called `set_compilation_breakdown`.

`json_round_trips_the_numbers_and_the_shape` asserts the document parses back
to the numbers that went in, that every category key is present, that the
delegated breakdown survives verbatim, and that the braces balance.
`thread_names_with_quotes_do_not_break_the_json` asserts a thread name
containing `"`, `\n` and `\t` is escaped rather than emitted raw.

---

## 7. The stderr summary line

One line, in the `key=<digits>` shape
`regression-suite/perf/run-cratonbench-gate.sh` already scrapes for
`compiles: c1=…` and `[GC-SUMMARY] …`:

```
[PHASE-ACCOUNTING] level=coarse schema=1 basis_wall_ns=4127801200 attributed_ns=4114947200 open_ns=0 unattributed_ns=12854000 over_attributed_ns=0 residual_ppm=3114 reconciles=1 threads=4 out_of_order=0 threads_dropped=0 vm_startup_ns=311208400 class_load_ns=88110300 verification_ns=…
```

Every value except `level` is an integer, deliberately: the gate's extraction
is `grep -oE 'key=[0-9]+' | grep -oE '[0-9]+$'`, and a decimal point splits the
token in half. `reconciles` is `1`/`0` for the same reason.
`the_summary_line_is_integer_only_so_the_gate_can_scrape_it` asserts this
property mechanically, so a future field cannot quietly break the gate.

The per-category values on this line are the **basis thread's**, not the
cross-thread totals, because that is what reconciles against the benchmark's
wall clock. (With no basis thread claimed, they fall back to the totals rather
than printing zeroes.)

---

## 8. The JFR sink

`phase::write_jfr_report` builds its own `EventTypeRegistry` and calls
`jdk_chunk::dump_to_file` directly, so a phase report can be produced by a run
that never started a flight recording — which, per the LIVENESS block in
`jfr/src/lib.rs`, is every run today.

The bytes are the **JDK's own** chunk format, which is what makes the JMC
caveat below meaningful: a JMC user opening the dump gets a real timeline
rather than an `IOException: Unknown string encoding 17` from CratonVM's own
internal format. `jfr/src/dump.rs` still owns that internal format and its
matching Rust reader; see the module docs at the top of `jfr/src/jdk_chunk.rs`
for why the two coexist.

Three event types:

| Type | Fields |
|---|---|
| `cratonvm.PhaseAccountingCategory` | `category`, `threadName`, `selfTime`, `count`, `primary` |
| `cratonvm.PhaseAccountingThread` | `threadName`, `wallTime`, `attributedTime`, `openTime`, `unattributedTime`, `overAttributedTime`, `primary` |
| `cratonvm.PhaseAccountingSummary` | `level`, `wallTime`, `attributedTime`, `openTime`, `unattributedTime`, `overAttributedTime`, `residualPpm`, `reconciles` |

A category event's *duration* is its self time; its *position* is nominal. A
category is by construction the union of many disjoint intervals and JFR has no
shape for that, so the duration is the number the report is about and the start
time is the owning thread's registration time. This is documented on
`report_to_events` as well, because a JMC user reading the timeline will
otherwise assume the bar is an interval.

`register_phase_events` is public so an operator who *does* have a live
`FlightRecorder` can put the phase events in the same chunk. It is deliberately
**not** called by `create_flight_recorder`: these are not `jdk.*` built-ins, and
growing the built-in registry would change the metadata section of every
existing recording (and `jfr/src/builtin.rs`'s `t6_total_event_count_47`).

`the_jfr_sink_writes_a_readable_chunk` round-trips the file through
`jdk_chunk::read_chunk` and asserts every decoded event names a registered type —
the same invariant `fuzz/fuzz_targets/fuzz_jfr_chunk.rs` asserts against
adversarial input for the internal format. `read_chunk` is a real decoder for the
JDK's format, not a mirror of the writer: the writer is separately validated
against the JDK's own `RecordingFile` and `jfr summary`, so a `cargo test` with
no JDK present still checks content rather than only self-consistency.

---

## 9. Verifying the 2% criterion on a real run

```sh
export CRATONVM_PHASE_ACCOUNTING=coarse
export CRATONVM_PHASE_ACCOUNTING_OUT=/tmp/phases.json
./target/release/cratonvm -cp bench CratonBench hashmap
```

Then either read the stderr line, or:

```sh
python3 - <<'PY'
import json
r = json.load(open("/tmp/phases.json"))
wall = r["basis_wall_ns"]
print(f"basis wall     {wall/1e6:9.2f} ms")
for name, ns in sorted(r["primary"]["categories"].items(), key=lambda kv: -kv[1]):
    if ns:
        print(f"  {name:<20} {ns/1e6:9.2f} ms  {100*ns/wall:5.2f}%")
print(f"  {'UNATTRIBUTED':<20} {r['unattributed_ns']/1e6:9.2f} ms  "
      f"{100*r['unattributed_ns']/wall:5.2f}%")
assert r["over_attributed_ns"] == 0, "over-attribution: the wiring is wrong"
assert r["anomalies"]["out_of_order_closes"] == 0, "a span outlived its scope"
print("reconciles:", r["reconciles"], f"({r['residual_ppm']/10000:.3f}%)")
PY
```

The criterion is met when `residual_ppm <= 20000` (2%), which is exactly what
`reconciles` reports.

**Three checks that must pass before the number means anything**, in this
order:

1. `over_attributed_ns == 0`. Non-zero means a span was charged time outside
   its thread's life — a wiring bug, and every other number is suspect.
2. `anomalies.out_of_order_closes == 0`. Non-zero means a call site holds a
   span past its logical scope, so its category is being credited with a
   neighbour's time.
3. `anomalies.open_spans` is small and expected. The report is normally taken
   inside a `vm_shutdown` span, so `1` is correct; a large number means spans
   are leaking.

**Follow the methodology.** `docs/benchmarking/methodology.md` applies
unchanged: one benchmark phase per process, at least 7 runs, compare checksums
before comparing times. A phase report from a run that failed the reliability
gate is worth exactly as much as that run's timing — which is nothing. Turning
phase accounting on is itself a workload change; do not compare a
phase-accounting run's wall clock against a baseline measured without it.

---

## 10. Wiring plan

`jfr/src/phase.rs` is the mechanism. **It has no call sites yet** — nothing
outside the crate opens a span, so a run today reports 100% unattributed. This
section is the ordered list of edits that make it real. Every item is outside
`jfr/`.

Line numbers are against `feat/c2-review-remediation` at `a8ce5a2f9`; every one
below was re-verified against that commit. Several crates are under concurrent
edit, so match on the quoted **anchor text**, not the number.

### 10.1 DONE: the three flags are declared

`types/src/flag_groups.rs:425-427` carries the three `INVENTORY` rows, and
`types/tests/flag-surface.txt:541-543` the matching fixture lines:

```rust
E { group: Group::DBG, token: "phase-accounting", on_key: Some("CRATONVM_PHASE_ACCOUNTING"), off_key: None, off_word: None },
E { group: Group::DBG, token: "phase-accounting-jfr", on_key: Some("CRATONVM_PHASE_ACCOUNTING_JFR"), off_key: None, off_word: None },
E { group: Group::DBG, token: "phase-accounting-out", on_key: Some("CRATONVM_PHASE_ACCOUNTING_OUT"), off_key: None, off_word: None },
```

`Group::DBG` is the right group: phase accounting is class (a) in the flag
census — no token here can change a program's result.

Nothing to do. Listed because it is a prerequisite a future edit must not
undo: removing these rows silently changes the flags' semantics (a declared
name is served from the latched snapshot, an undeclared one from live
`getenv`) *and* fails `flag_declaration_guard`.

### 10.2 BLOCKING: add the `cratonvm-jfr` dependency to three crates

`jfr` depends only on `cratonvm-types` (plus `parking_lot`, `rustc-hash`,
`smallvec`, `tracing`), so these edges cannot cycle. `vm` and `vm-cli` already
reach `phase` (`vm` depends on `cratonvm-jfr`; `vm-cli` can go through
`cratonvm_vm`, or add the direct edge).

| File | `[dependencies]` at | Edit |
|---|---|---|
| `jit/Cargo.toml` | line 16 | add `cratonvm-jfr = { path = "../jfr", version = "0.3.0" }` |
| `gc/Cargo.toml` | line 16 | add `cratonvm-jfr = { path = "../jfr", version = "0.3.0" }` |
| `classloading/Cargo.toml` | line 19 | add `cratonvm-jfr = { path = "../jfr", version = "0.3.0" }` |
| `vm-cli/Cargo.toml` | line 44 | add `cratonvm-jfr = { path = "../jfr", version = "0.3.0" }` |

`gc`'s existing `cratonvm-jit` edge is `[dev-dependencies]` only, and `jit`'s
`cratonvm-gc` edge likewise, so neither is affected.

### 10.3 Process boundaries — `vm-cli`

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 1 | `vm-cli/src/main.rs:4568` | immediately after `cratonvm_types::flag_groups::expand_process_env();` in `main` | `cratonvm_jfr::phase::mark_process_start();` — must be after group expansion (the flag is a `CRATONVM_DBG` token) and before anything else records |
| 2 | `vm-cli/src/main.rs:31` | inside `maybe_dump_jit_method_stats`, after `dump_method_stats_to_stderr()` | publish the delegated breakdown and emit the sinks (see snippet below) |

Item 2 lands inside the existing once-only `DUMPED` guard, which already
handles both shutdown routes (`System.exit` via the pre-exit hook at
`vm-cli/src/main.rs:2402`, and a normal `main` return at
`vm-cli/src/main.rs:4832`). Rename the function to
`maybe_dump_shutdown_reports` while you are there — it is no longer only about
JIT method stats.

```rust
    // Delegate the compile-phase breakdown rather than re-timing it; see
    // docs/observability/phase-accounting.md §1. `MetricsSummary::phase_totals_ns`
    // (jit/src/metrics.rs:1217) is already `Vec<(&'static str, u64)>`, which is
    // exactly the argument shape.
    let summary = cratonvm_jit::metrics::summary();
    cratonvm_jfr::phase::set_compilation_breakdown(&summary.phase_totals_ns);
    if let Some(outcome) = cratonvm_jfr::phase::emit_configured_sinks() {
        eprintln!("{}", outcome.report.summary_line());
        if let Some((path, Err(e))) = &outcome.json {
            eprintln!("[cratonvm] phase-accounting JSON sink {path:?} failed: {e}");
        }
        if let Some((path, Err(e))) = &outcome.jfr {
            eprintln!("[cratonvm] phase-accounting JFR sink {path:?} failed: {e}");
        }
    }
```

Note the guard: `maybe_dump_jit_method_stats` currently runs only when
`cratonvm_types::flags().jit.method_stats` is set. The phase block must sit
**outside** that condition (but inside the `DUMPED` compare-exchange), or the
report would require an unrelated flag.

### 10.4 VM startup and shutdown

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 3 | `vm-cli/src/main.rs:3188` | `let mut vm = Vm::new(config);` | wrap: `let _p = cratonvm_jfr::phase::enter(Category::VmStartup); let mut vm = Vm::new(config); drop(_p);` — a block is cleaner than a `drop`, but `vm` must outlive the span |
| 4 | `vm/src/vm/vm_init.rs:6575` | first statement of `pub fn new(config: VmConfig)` | alternative to #3 if `vm-cli` is not the only launcher: `let _p = cratonvm_jfr::phase::enter(Category::VmStartup);` at the top of the function body. Do **one** of #3 / #4, not both — they would nest, which is correct but pointless |
| 5 | `vm-cli/src/main.rs:4829` | immediately after `let result = run();` | `let _p = cratonvm_jfr::phase::enter(Category::VmShutdown);` held to the end of the block, so teardown is charged rather than unattributed |

`libcratonvm` embedders bypass `vm-cli` entirely; #4 is the variant that covers
them, and `mark_process_start()` then has to move into
`libcratonvm`'s init as well.

### 10.5 Execution — the coarse span

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 6 | `vm-cli/src/main.rs:4161` | around the `invoke_on_class_shared(...)` call that enters the application's `main` | `let _p = cratonvm_jfr::phase::enter(Category::JavaExecution);` for the duration of the call. This is the single span that makes `coarse` reconcile |

Site 6 is the load-bearing one for the 2% criterion: everything else nests
inside it and subtracts.

### 10.6 Execution — the fine split

Guard each of these on `phase::fine_enabled()` before constructing anything;
`enter` alone returns a non-recording span at `coarse`, but these are per-call
sites and should not even branch into the span constructor.

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 7 | `vm/src/runtime/interpreter.rs:5723` | top of `pub fn execute(...)` body, after the early `disable_jit`/trace guards | `let _p = cratonvm_jfr::phase::enter(Category::Interpretation);` |
| 8 | `vm/src/jit/helpers.rs:1058` | inside `try_call_compiled_entry`, around the `try_call_compiled_entry_inner(...)` call at line 1070 | `Category::JitExecution` |
| 9 | `vm/src/jit/helpers.rs:1257` | inside `try_call_compiled_entry_reentrant`, around the `try_call_compiled_entry(...)` call at line 1305 | `Category::JitExecution` |

Sites 8 and 9 nest (9 calls 8), which self-time accounting handles: the inner
charge is subtracted from the outer.

### 10.7 Class loading and verification

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 10 | `classloading/src/class_manager.rs:3858` | top of `pub fn load_class` | `Category::ClassLoad` |
| 11 | `classloading/src/class_manager.rs:4104` | top of `pub fn define_class` | `Category::ClassLoad` |
| 12 | `classloading/src/verifier.rs:163` | top of `pub fn verify_class` | `Category::Verification` |
| 13 | `classloading/src/verifier.rs:232` | top of `pub fn verify_class_bytecode` | `Category::Verification` |
| 14 | `vm/src/vm/vm_util.rs:853` | top of `fn initialize_class_shared` — the `<clinit>` driver whose two exits already emit `jdk.ClassLoad` (`vm_util.rs:1362`, `vm_util.rs:2009`) | `Category::ClassLoad` |

Sites 12/13 nest inside 10/11, so `verification` is subtracted from
`class_load` rather than added to it. Site 14 nests around the `<clinit>`
interpretation, which at `fine` subtracts back out into `interpretation`.

### 10.8 Compilation, installation, deoptimization

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 15 | `jit/src/lib.rs:8957` | next to `let metrics = metrics::CompileRecorder::begin(...)` in `try_compile_inner` | `let _p = cratonvm_jfr::phase::enter(Category::Compilation);` — same lifetime as the recorder, which is what makes `compilation_delta_ns` meaningful |
| 16 | `jit/src/tiered.rs:1552` | in `compiler_loop`, around the `core.wake.wait(&mut q)` call (the inner `loop` that parks) | `Category::Idle` — without this the compiler thread reads as ~100% unattributed and drags the cross-thread aggregate down |
| 17 | `vm/src/runtime/interpreter/invoke.rs:18401` | top of `background_compile_task` | `Category::Compilation` (the VM-side half of the compile the worker runs) |
| 18 | `vm/src/runtime/interpreter/invoke.rs:17303` | top of `try_jit_compile_callee` | `Category::Compilation` if compiled synchronously, `Category::CompileQueueWait` around any blocking wait for a background result |
| 19 | `vm/src/runtime/interpreter/invoke.rs:18146` | the `emit_compilation_event_arc` site — the publication point in `try_jit_compile_callee` | `Category::CodeInstall`, around the cache insert / entry publication that precedes it |
| 20 | `vm/src/jit/helpers.rs:9920` | top of `pub fn deoptimize` — the site that already emits `jdk.Deoptimization` at line 10115 | `Category::Deoptimization` |

Site 15 is inside `jit`, site 17 inside `vm`; they nest when the worker calls
through, and the inner charge is subtracted from the outer. Do not "fix" that
by removing one — 17 minus 15 is exactly the VM-side overhead around a compile,
which is a number worth having.

### 10.9 GC and safepoints

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 21 | `vm/src/runtime/interpreter.rs:1182` | top of `pub(crate) fn maybe_gc` — the mutator's GC entry, which already emits `jdk.GarbageCollection` at line 1352 | `Category::GcPause` |
| 22 | `vm/src/runtime/interpreter.rs:1547` | top of `pub fn maybe_gc_forced_pub` | `Category::GcPause` |
| 23 | `gc/src/g1.rs:2273` | top of `pub fn young_collection` | `Category::GcPause` |
| 24 | `gc/src/g1.rs:2736` | top of `pub fn mixed_collection` | `Category::GcPause` |
| 25 | `gc/src/gen_heap.rs:3802` | top of `pub fn collect_garbage_with_finalizers` | `Category::GcPause` |
| 26 | `gc/src/concurrent_mark.rs:492` | top of `pub fn concurrent_mark` | `Category::GcConcurrent` |
| 27 | `vm/src/runtime/interpreter.rs:1247` | the STW region opened by `StopTheWorldToken::new()` — and its siblings at `:1461`, `:1640`, `:1698`, `:1872`, `:1941`, `vm/src/vm.rs:62270`, `vm/src/vm.rs:62331` | `Category::Safepoint` when the token is taken for something other than a collection; leave the GC ones to #21–#25 rather than nesting a second span |

Sites 23–25 are inside `gc` and nest inside 21/22; that is correct and gives
"time in `maybe_gc` that is not in the collector proper" for free.

### 10.10 Native transitions

| # | File:line | Anchor | Edit |
|---|---|---|---|
| 28 | `vm/src/vm/vm_exec.rs:1338` | top of `fn safe_native_call_impl` | `let _p = cratonvm_jfr::phase::enter(Category::NativeCall);` |

**One site, not several.** `safe_native_call_impl` is the single choke point:
`safe_native_call` (`vm/src/vm/vm_exec.rs:1316`) and
`safe_native_call_prevalidated_objects` (`:1329`) are both one-line wrappers
around it, and every interpreter dispatch path reaches it —
`invoke_cached_native_callback_impl`
(`vm/src/runtime/interpreter/invoke.rs:4537`) calls one or the other, and
`invoke_cached_intrinsic` (`:12420`) reaches it too, as its own doc comment
records ("`safe_native_call` already records the native ring enter/exit").
Instrumenting the two dispatch sites separately instead would miss any third
caller and would nest redundantly.

This is the highest-frequency instrumentation point in the plan. Measure it: if
two clock reads per native call moves the benchmark, demote `native_call` to
`fine` by adding it to `Category::active_at`'s `Level::Fine` arm.

### 10.11 Ordering

Do them in this order; each step leaves the tree in a state where the report is
truthful, just coarse:

1. §10.1 + §10.2 (unblock the build and the test suite)
2. §10.3 + §10.4 + §10.5 — at this point `vm_startup` + `java_execution` +
   unattributed already covers 100% of the run, and the residual is a real
   measurement of "everything not yet wired"
3. §10.7 + §10.9 — the two largest expected residual consumers
4. §10.8 + §10.10
5. §10.6 last, and only under `fine`

After step 2, record the residual. Each later step should shrink it, and a step
that does **not** shrink it is telling you its category is genuinely empty on
that workload — which is a finding, not a wiring failure.

---

## 11. What remains unattributable, and why

Stated rather than papered over. With the whole of §10 wired, these still land
in `unattributed_ns`:

1. **The instrumentation's own overhead.** Two `Instant::now()` calls and a
   thread-local borrow per span. The clock read that *ends* a span happens
   before the charge is computed, so the bookkeeping after it is uncharged.
   Order 100 ns per span; at `coarse` that is negligible, at `fine` it is the
   dominant residual and is why `fine` is not the gate level.

2. **Time before `mark_process_start()`.** Process startup up to and including
   `expand_process_env` — the allocator init, `clap` parse, argfile expansion,
   crash-handler installation. It is outside the epoch, so it is not in
   `process_wall_ns` at all rather than being unattributed. A benchmark's
   reported wall time is the harness's `time`, which *does* include it, so this
   is a systematic under-measurement of a few milliseconds. Moving
   `mark_process_start()` earlier is not possible: the flag is a `CRATONVM_DBG`
   token and is not readable until `expand_process_env` has run.

3. **Scheduler preemption and OS-level stalls.** A thread descheduled inside a
   span has that time charged to the span's category (it is wall time, and the
   span was open). A thread descheduled *between* spans has it charged to
   unattributed. Neither is wrong, but it means the residual is
   machine-load-sensitive — run the reliability gate first
   (`docs/benchmarking/reliability-gate.md`).

4. **Threads past `MAX_TRACKED_THREADS`.** Reported as
   `anomalies.threads_dropped`, and they affect only the cross-thread
   aggregate, never the basis thread's reconciliation.

5. **Dispatch between executor spans, at `fine` only.** The interpreter's
   invoke machinery — resolution, argument marshalling, frame setup — sits
   outside both `interpretation` and `jit_execution` unless a span is opened
   around it too. Deliberately left out: instrumenting it would double the
   per-call clock reads.

6. **`libcratonvm` embedders.** Nothing in the C ABI path calls
   `mark_process_start`, so an embedded VM reports from its first span. Fixing
   this is a `libcratonvm` init edit, listed as the variant under §10.4.

Two things that are **not** in this list, because they are bugs rather than
limits: a non-zero `over_attributed_ns`, and a non-zero
`anomalies.out_of_order_closes`. If either appears, the wiring is wrong and the
residual is not interpretable.

---

## See also

* `jfr/src/phase.rs` — the implementation and its tests
* `docs/jit/compiler-metrics.md` — the per-compilation breakdown this report
  delegates to
* `audits/tlab-and-card-audit.md` — `gc_metrics_report()` and
  `collector_decision_report()`, which explain *why* the `gc_pause` bucket is
  the size it is
* `docs/benchmarking/methodology.md` — how to measure the wall clock this
  report partitions
* the C2 review — the review lane this closes
