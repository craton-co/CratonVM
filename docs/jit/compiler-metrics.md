# JIT compiler metrics

Structured, per-method compiler reports for the CratonVM JIT.

**Why this exists.** The C2 review
has a P0 lane *"Measure compilation quality"*, whose acceptance criterion is:

> Per-method compiler report is available without parsing debug logs.

Before `jit/src/metrics.rs`, the only way to find out what the compiler did to a
method was to set one of a dozen `CRATONVM_DBG_*` flags and scrape stderr — and
most of the quantities the review names (phase wall time, node counts across
passes, frame bytes, code bytes, deopt-metadata size) were never printed at all.

Everything below is produced by `cratonvm_jit::metrics`. Nothing in this
document requires reading a log line.

---

## Enabling it

| Flag | Default | Meaning |
|---|---|---|
| `CRATONVM_JIT_METRICS` | off | `1`/`true`/`yes`/`on` enables collection. Nothing is recorded otherwise. |
| `CRATONVM_JIT_METRICS_RING` | `256` | How many reports are retained in memory. `0` or unparseable → the default. |
| `CRATONVM_JIT_METRICS_OUT` | unset | Path to append one JSON object per line, per compilation. |

These are read through `cratonvm_types::flags::runtime_var_os` (the live
environment), matching `jit/src/ir_verify.rs`: they are **not** declared
`VmFlags` names, so `-XX:` syntax does not reach them and a `set VAR=…` before
launch does.

```sh
CRATONVM_JIT_METRICS=1 CRATONVM_JIT_METRICS_OUT=/tmp/jitc.jsonl cratonvm MyApp
```

Both the enable flag and the ring size are latched on first read, so changing
them mid-run has no effect.

## Reading it

```rust
use cratonvm_jit::metrics;

// The most recent compilation, or the most recent one for a given method.
let last = metrics::last_compilation_report(None);
let hash = metrics::last_compilation_report(Some("java/lang/String.hashCode"));

// Everything retained, oldest first.
for report in metrics::compilation_reports() {
    println!("{}", report.to_json());
}

// Aggregate over the retained reports plus the process-wide bailout table.
println!("{}", metrics::summary().to_json());
```

The filter passed to `last_compilation_report` is a substring test against
`Class.name descriptor` (`CompilationReport::method_key`). Anchor it with a
trailing `.` or a descriptor fragment if a prefix is ambiguous (`Probe7` also
matches `Probe70`).

---

## Field inventory

### Identity and routing

| Field | Unit | Source |
|---|---|---|
| `seq` | count | Assigned at publication. A gap between consecutive retained reports means the ring wrapped. |
| `class` / `method` / `descriptor` | — | `CachedBytecodeMethod`. |
| `tier_requested` | `"c1"` / `"c2"` | `try_compile_inner`'s `optimize` parameter, **before** `CRATONVM_JIT_FORCE_C2` is folded in. |
| `path` | `optimizing` / `single_pass` / `not_entered` | Which backend was reached. On an installed method this is the artifact's own `used_ir_backend`, so it cannot drift from the truth. |
| `fell_through_to_single_pass` | bool | The optimizing pipeline was entered **and then declined**. This is what separates "C2 produced no bodies" from "C2 was never asked". |
| `admission` | string / `null` | The admission verdict — the same sentence `CRATONVM_DBG_IR_COMPILES` prints, e.g. `"optimize=false — the C1/fast tier was requested, not C2"` or `"admitted to the optimizing pipeline"`. |
| `outcome` | enum | See below. |

`outcome` values:

* `installed` — a `CompiledMethod` was produced.
* `bailed_out` — abandoned, with at least one structured bailout recorded.
* `abandoned` — abandoned with **no** structured reason. The size of this
  bucket is itself a finding: it counts the failure paths that still signal
  with a bare `Option::None` instead of a `BailoutReason`.
* `in_progress` — never appears on a published report.

### Bailouts

`bailouts` is an array (bounded at 16 per report) of
`{ "category", "phase", "detail" }`:

* `category` — the same stable string `bailout::Bailout::category()` returns,
  so it joins directly against `bailout::bailout_counts()`.
* `phase` — the pipeline point that produced it: `"post-build"`,
  `"post-optimize"`, `"post-escape-analysis"`, `"pre-lower"`.
* `detail` — `Bailout`'s `Display`, including any context string.

This module **attributes**; it does not count. `bailout::record_bailout` still
owns the process-wide counters, and `metrics::note_current_bailout` deliberately
does not bump them — the call site in `lib.rs::ir_verify_reject` already does,
and double counting would corrupt the table the summary reports.

### Phases

`phases` is an array with one entry per phase, **always all of them**, in
pipeline order:

`scan`, `build`, `optimize`, `escape_analysis`, `verify`, `schedule`, `lower`,
`encode`, `install`, `single_pass`

Each entry is `{ "phase", "runs", "wall_ns", "nodes_before", "nodes_after" }`:

* `runs` — how many times the phase ran. `verify` runs up to three times per
  compilation (post-optimize, post-escape-analysis, pre-lower).
* `wall_ns` — nanoseconds summed over `runs`. `null` **iff** `runs == 0`.
* `nodes_before` / `nodes_after` — `graph.nodes.len()` (arena size, including
  `Op::Dead`) around a graph-mutating phase; `null` where the phase does not
  touch the graph.

### Shape and size

| Field | Unit | Notes |
|---|---|---|
| `nodes_built` | nodes | Arena size right after `IrBuilder::build`. |
| `nodes_at_lower` | nodes | Arena size handed to `ir_lower::lower_inner`. |
| `live_nodes_at_lower` | nodes | Non-`Op::Dead` subset of the above. |
| `peak_live_values` | values | **Not measured** — see below. |
| `spills` | count | **Not measured**. |
| `reloads` | count | **Not measured**. |
| `frame_bytes` | bytes | `CompiledMethod::frame_layout.frame_size` — what the prologue subtracts from RSP. |
| `code_bytes` | bytes | `CompiledMethod::code_bytes().len()` — the emitted bytes, not the mapped page capacity. |
| `ir_safepoints` | count | `graph.safepoints.len()` at lowering. Optimizing path only. |
| `oop_maps` | count | `CompiledMethod::oop_maps.len()`. Zero without `CRATONVM_PRECISE_JIT_MAPS` — that is a real property of the artifact (the GC falls back to a conservative scan), not a measurement gap. |
| `deopt_points` | count | `CompiledMethod::deopt_points.len()`. |
| `deopt_metadata_bytes` | bytes | Estimated resident size of the deopt-point vector plus the boxed points, plus each `FrameState`'s heap (method key, locals, stack, monitors, inlined caller chain). An estimate: a `FrameValue::VirtualObject` owns further heap this walk does not follow. |
| `code_cache_bytes_at_install` | bytes | `COMMITTED_JIT_CODE_BYTES` read at install — live code-cache occupancy including this method. |
| `total_wall_ns` | ns | Recorder construction to publication. Present on **every** published report, including bailouts. |

### `null` means "not measured", never zero

Every numeric field is a `Measured<T>`, which renders as JSON `null` when no
call site supplied it. This is deliberate and load-bearing: `"spills": 0` and
`"spills": null` are different claims, and conflating them would make every zero
in the report unfalsifiable. The same applies to phases — a phase that never ran
has `"runs": 0, "wall_ns": null`; a phase that ran and finished within clock
resolution has `"runs": 1, "wall_ns": 0`.

---

## What is measured, and what is not

| Metric | Status | Where |
|---|---|---|
| method identity, requested tier | **real** | `CompileRecorder::begin`, top of `try_compile_inner` |
| admission reason | **real** | the `ir_stage_reporting() \|\| metrics.is_enabled()` verdict block |
| path taken, fall-through | **real** | `enter_optimizing_pipeline` at the admission-`if` body; `enter_single_pass` after it |
| outcome + bailout attribution | **real** | `installed` at both success returns; `note_bailout_reason` at the graph-size gate; `note_current_bailout` in `ir_verify_reject` |
| `scan` time | **real** | around `x64::jit_scan` |
| `build` time + node count | **real** | around `IrBuilder::build` |
| `optimize` time + nodes before/after | **real** | around `ir_optimize::optimize` |
| `escape_analysis` time + nodes before/after | **real** | around `escape_analysis_from_ir` … `apply_ea_to_ir` |
| `verify` time (all three runs) | **real** | inside `ir_verify_reject` |
| `schedule` time | **real** | around `ir_schedule::schedule` |
| `lower` time | **real** | around `ir_lower::lower_inner` |
| `single_pass` time | **real** | around `x64::compile_with_param_slots` |
| frame bytes, code bytes, oop maps, deopt points/bytes, code-cache occupancy | **real** | `CompileRecorder::installed`, from the finished artifact |
| **per-optimization-pass** breakdown | **not measured** | `ir_optimize::optimize` runs GVN, DCE, reassociation, LICM and unrolling behind one entry point and publishes no per-pass boundary. Splitting it is a change to `ir_optimize.rs`. |
| `encode`, `install` phases | **not measured** | `ir_lower::lower_inner` and `x64::compile_with_param_slots` each select, encode and install inside one call. The phases exist so splitting them later is not a schema change. |
| `peak_live_values` | **not measured** | `regalloc` computes live ranges but publishes no peak. `CompileRecorder::set_peak_live_values` exists and has no call site. |
| `spills`, `reloads` | **not measured** | The optimizing lowerer does not allocate registers in the classical sense — it reserves one frame slot per graph node (see below) — and the single-pass backend's allocator does not export counts. Setters exist and have no call site. |
| deopt **events** at runtime | out of scope | `deopt::DeoptimizationLog` already owns that; this module reports the *metadata size* a compilation produced, not how often it fired. |

---

## Worked examples

### 1. Why did method X bail out?

```sh
CRATONVM_JIT_METRICS=1 CRATONVM_JIT_METRICS_OUT=/tmp/jitc.jsonl cratonvm MyApp
grep 'com/example/Hot.loop' /tmp/jitc.jsonl | tail -1 | python -m json.tool
```

Read three fields in order:

1. `admission` — if it names a declined conjunct (`"optimize=false …"`,
   `"precise exception frames required …"`, `"value shape not admitted …"`),
   the optimizing tier was never entered and there is nothing else to explain.
2. `fell_through_to_single_pass` — if `true`, C2 was entered and gave up.
   `bailouts[]` names why if the reason is structured.
3. `outcome` — `abandoned` with an empty `bailouts[]` means the compile died on
   a path that still signals with a bare `None` (a constant-pool resolver miss,
   an `IrBuilder::build` refusal, a lowerer bail). The `phases[]` row with the
   last non-null `wall_ns` tells you how far it got.

That last case is the honest limitation and the actionable one: the count of
`abandoned` outcomes in `summary().by_outcome` is a direct measure of how much
of the pipeline still lacks a `BailoutReason`.

### 2. Which pass dominates compile time?

```rust
let s = cratonvm_jit::metrics::summary();
for (phase, ns) in &s.phase_totals_ns {
    println!("{phase:>16} {:>10} µs", ns / 1_000);
}
```

Check `phase_runs` alongside it. A phase with `0` runs contributes `0` ns, and
that zero means "never ran", not "free" — `encode` and `install` are always in
that state today.

Note that `verify` accumulates up to three runs per compilation, and is only
non-zero when `ir_verify::verify_enabled()` (debug builds, or
`CRATONVM_JIT_VERIFY_IR=1`). If `verify` dominates a release profile, someone
left that flag on.

### 3. Is frame size tracking node count rather than live values?

This is the question the review's frame-size item asks, and the report answers it
arithmetically. `ir_lower::estimate_frame_bytes` is:

```
frame = locals*8 + context*8 + 32 (bookkeeping) + max_nodes*8
      + max_call_args*8 + 16 (stack-arg reserve) + 32 (shadow)
```

— where `max_nodes` is `graph.nodes.len()`, the **arena** size, with no regard
for live-range overlap (`ir_lower.rs` carries a `TODO(liveness-slot-reuse)` on
exactly that line). So for any report with `path == "optimizing"`:

```
frame_bytes  ≈  nodes_at_lower * 8  +  (locals + call args + ~80 bytes)
```

If `frame_bytes / 8` tracks `nodes_at_lower` rather than
`live_nodes_at_lower`, frame size is being driven by arena size. The gap
`nodes_at_lower − live_nodes_at_lower` is dead arena the optimizer left behind,
and every one of those nodes is still costing 8 frame bytes. Sorting the ring by
that gap gives the worst offenders directly:

```rust
let mut rs = cratonvm_jit::metrics::compilation_reports();
rs.retain(|r| r.nodes_at_lower.is_measured());
rs.sort_by_key(|r| {
    r.nodes_at_lower.or(0) as i64 - r.live_nodes_at_lower.or(0) as i64
});
```

`peak_live_values` is `null` precisely because nothing computes the number this
should be compared against; that is the measurement to add next, and it belongs
in `regalloc`.

### 4. Is the code cache filling up, and with what?

`code_cache_bytes_at_install` is monotone within a run except where invalidation
reclaims bodies. Plotting it against `seq` shows growth; sorting reports by
`code_bytes` shows which methods are responsible. `frame_bytes` next to
`code_bytes` distinguishes "large method" from "large frame for a small method",
which is the signature of the arena-sized frame above.

---

## Cost

* **Disabled** (the default): `CompileRecorder::begin` is one relaxed atomic
  load and returns a handle holding `None`. No allocation, no clock read, no
  thread-local access. Every hook — `phase`, `phase_nodes`, `set_admission`,
  `installed`, `note_bailout*` — is an `Option` test that returns immediately;
  `phase` does not even call `Instant::now`. The one change visible on the
  default path is the extra `metrics.is_enabled()` disjunct on the admission
  verdict block, which is the same relaxed load.
* **Enabled**: one `Rc<RefCell<CompilationReport>>` allocation per compilation
  (the report holds one `Vec` of 10 phase rows and three `String`s), two
  `Instant::now` calls per timed phase, the admission verdict `String` (which
  was previously built only under `CRATONVM_DBG_IR_COMPILES`), plus one
  `Vec<Node>` scan at lowering to count non-`Op::Dead` nodes. With
  `CRATONVM_JIT_METRICS_OUT` set, one `writeln!` under a mutex per compilation.
  All of it is off the *execution* path — it is per-compile, not per-call.

Metrics failure never affects compilation. Every borrow is a `try_borrow`, every
file write discards its error, an unopenable `CRATONVM_JIT_METRICS_OUT` path is
silently ignored, and the recorder publishes from `Drop` so no compiler control
flow depends on it.

---

## Design notes

**Publication from `Drop`.** `try_compile_inner` has roughly forty `return None`
exits — every `jitc_bail!` resolver miss, the `jit_scan` reject, the `?` on the
single-pass backend. Recording at each of them would have meant forty edits and
forty chances to miss one. The recorder publishes in its destructor instead, so
every exit path produces a report and not a single control-flow edge changed.

**Thread-local for `ir_verify_reject`.** That helper is called from three places
inside `try_compile_inner` and the task constraint was not to change signatures,
so it charges its time and its bailout to the innermost in-flight compilation on
the current thread. The active set is a *stack*, not a slot: `callee_compiler`
re-enters `try_compile` for inlining candidates, so compilations nest and the
callee must not be billed to the caller.

**Summary derived from the ring.** `summary()` aggregates the retained reports
rather than keeping independent counters, so `summary().by_outcome` and
`compilation_reports()` can never disagree. The cost is that eviction is lossy —
compare `total_recorded` against `retained` to see whether the ring wrapped.
`bailout_categories` is the exception: it is `bailout::bailout_counts()`
verbatim, which is process-wide, unbounded, and fed whether or not metrics are
enabled.

## Related

* `jit/src/metrics.rs` — the implementation and its tests.
* `jit/src/bailout.rs` — `BailoutReason`, the category table, `bailout_counts()`.
* `jit/src/ir_verify.rs` — the verifier whose rejections show up as
  `ir_verification` bailouts.
* the C2 review — the review this closes a P0 item of.
