# WORKER-5 NOTE 6 — the three residual instrument defects the brief listed, and the one that turned out to be 6.9× rather than 5%

**Status: FIXED, MEASURED.** Lane WORKER-5, 2026-08-21/22. Two harness fixes
(`regression-suite/harness-census.sh` + three hunks in `run.sh`) and two VM
fixes (`cratonvm_types::flags::resolve_capped_usize`, and the `cratonvm_jit`
drop counter of §4a), both built and verified on Linux.

> **§4a, added 2026-08-22, closes the gap §3 and §5 left open.** All three of
> the report's bounded collections now report `truncated` + `dropped`, so a
> strict run's saturation verdict can say **totals** instead of `UNKNOWN`.

§4 of the WORKER-5 brief lists this week's measurement failures. Three of them
were still open. They are all in the arithmetic and all in the same direction:
**each makes a run report more certainty than it has.**

---

## 1. `CRATONVM_NATIVE_SHADOW_SINK_CAP=200000` is silently discarded — and what replaces it is SMALLER than the ceiling

Both sinks read the shared knob as

```rust
runtime_var(NAME).ok().and_then(|v| v.trim().parse().ok())
    .filter(|n| *n > 0 && *n <= 65_536).unwrap_or(4096)
```

A value ABOVE the ceiling therefore falls back to the **default**. An operator
asking for 200,000 gets **4,096** — an order of magnitude less than the ceiling
they were trying to exceed — with nothing printed. They re-read
`truncated: true` and conclude the knob does not work.

**It is self-inflicted.** `vm-cli/src/main.rs:2999` prints, on saturation,

> `Re-run with CRATONVM_NATIVE_SHADOW_SINK_CAP=(recorded + dropped) * 2`

which exceeds 65,536 for any workload with more than ~32,768 shadows. **The VM
can print advice it then throws away.**

### 1.1 The fix, and why the two halves are asymmetric

One resolver both sinks share — `cratonvm_types::flags::resolve_capped_usize` —
because `vm/src/vm/vm_exec.rs` and `vm/src/jit/helpers.rs` held the same twelve
lines and could drift while answering ONE knob.

* **too big → CLAMP to the ceiling.** The ceiling's stated purpose is that "a
  mistyped value cannot turn a diagnostic sink into a memory leak". Clamping
  preserves that exactly, and gives the operator the largest value the rule
  allows — strictly closer to intent than the default.
* **0 / empty / non-numeric → the default, unchanged.** There is no closer
  value, and a cap of zero would report an empty population as a complete one.
* **Either way, one `[cratonvm] warning:` line**, naming the value asked for,
  the value in force, and why. The sink name is a parameter so two callers
  sharing one knob produce two distinguishable lines.

### 1.2 VERIFIED at runtime, all six arms

Built on Linux (`/data/cratonvm-w5im.bin`, `cargo build --release -p
cratonvm-cli`), each arm a fresh process writing its own `--jdk-only-report`;
the `cap` field of `observation_sink` is the value actually in force:

| `CRATONVM_NATIVE_SHADOW_SINK_CAP` | report `cap` | warned |
|---|---:|---|
| unset | 4096 | — |
| **200000** | **65536** (was 4096) | yes, both sinks |
| 65536 | 65536 | — |
| 9000 | 9000 | — |
| 0 | 4096 | yes |
| `banana` | 4096 | yes |

Five unit tests in `types/src/flags.rs` cover the same matrix plus
`both_sinks_resolve_one_knob_the_same_way`; `too_big_is_clamped_not_defaulted`
asserted 4096 by construction until today. `cargo test -p cratonvm-types --lib
flags::` → 50 passed, 0 failed, and the five new names appear in the output —
checked, because an `rc=0` from a filter that matched nothing is the standing
hazard.

**One thing this record got wrong on the way in, and it is the reusable part:**
the first build rendered the warning as
`ceiling of 65536;                  CLAMPED to 65536`. The patch script was
Python, and a Python `\`+newline joins the lines while KEEPING the next line's
indentation inside the Rust literal — the mirror image of the heredoc hazard.
**It was caught by RUNNING the binary, not by reading the diff**, and a `cat -A`
of the patched line had already been done and passed, because the damage was in
a string literal rather than in the control characters that check looks for.

## 2. `sort -u` over whole JSON lines is not a union over triples — and "the contract working" is 70, not 481

`run.sh` unions the per-vector reports with a whole-line `sort -u`, arguing:

> "Rows are byte-identical across vectors for the same fact — `summary` is a
> pure function of the other fields — so `sort -u` is a real UNION and not an
> approximation."

`summary` **is** a pure function of the other fields. The premise fails one
field earlier: **`native_kind` and `outcome` are properties of a DISPATCH, not
of a triple.** The same method dispatches differently in different vectors, so
`sort -u` unions `(triple, native_kind, outcome)`.

MEASURED on 105 real reports (`SUITE=all CRATONVM_ARGS=--jdk-only`,
cratonvm-r10.exe), with two independent implementations (Python and the awk that
shipped) agreeing to the row:

```text
whole-line distinct rows          1868
distinct TRIPLES                  1457
triples appearing more than once   387   (all differ in native_kind;
                                          385 also differ in outcome)

native-won     run.sh 1387   triples 1387   double-counted   0
bytecode-won   run.sh  481   triples  455   double-counted  26
```

The brief predicted "477 → 453, 24 double-counted"; at this tip it is
481 → 455, 26. Same defect, different day.

**But the double-count is the small half.** The two buckets are not disjoint:

```text
native-won triples             1387
bytecode-won triples            455
in BOTH                         385
bytecode-won and NEVER native    70
```

`run.sh` labels its 481 **"bytecode-won (the contract working)"**. The contract
worked, for a triple that never also ran the native, **70 times**. The other 385
ran the native somewhere else in the same suite — which is the defect the census
exists to count. **A 6.9× overstatement of the good news**, in the line most
readers stop at.

The run now prints all of it: the triple counts, the overlap, the
never-native figure labelled as the only one that means "the contract working",
and the old whole-line numbers so a record quoting 481 can be reconciled.

## 3. The saturation grep cannot match the one sink that is unmeasured

`run.sh` asked `grep -l '"truncated": true'` and, finding none, printed

> `saturation: none — no report truncated a bounded collection, so the counts
> above are totals, not floors.`

The report has THREE bounded collections. The third, `jit_compile`, lives in
`cratonvm_jit` and has no counter, so `vm_init.rs` renders it
`"truncated": null` **on purpose** — its own comment says *"an unmeasured thing
must not render as a clean one"*. `= true` matches neither `false` nor `null`,
so the harness converted a deliberate `null` into a positive claim of
completeness.

**MEASURED: all 105 reports carry a `"truncated": null`.** So that line has been
wrong on every strict run there has ever been — not occasionally, always.

The fix is a third verdict, not a guess:

| reports say | verdict |
|---|---|
| any `true` | `SATURATED` — floors, and name the nulls too |
| any `null`, no `true` | **`saturation: UNKNOWN`** — NOT known to be totals |
| all `false` | `saturation: none` — totals |

## 4. Where the code is, and how it fails

`regression-suite/harness-census.sh` holds both harness fixes and all their
tests; `run.sh` gains a `.` and two call sites. `--selftest` needs no VM, no
JDK and no reports:

```text
  ok   one triple with two outcomes counts as ONE triple, in BOTH buckets
  ok   a never-native triple is counted as bytecode_only
  ok   single row · empty input is all zeros
  ok   descriptors with / ; and [ parse as distinct triples
  ok   all-false -> totals (rc=0)
  ok   a null sink -> UNKNOWN (rc=1), not 'none'
  ok   true wins over null, and the null is still reported
  ok   an EMPTY dir reports none
  ok   the premise holds: `= true` finds nothing in a null-carrying set
```

The last line is a **premise pin**, like `harness-vmfault.sh`'s: if a future VM
starts rendering that sink `false`, the check goes red and the UNKNOWN branch
can be retired deliberately rather than left as folklore.

## 4a. CLOSED 2026-08-22 — `jit_compile` has a counter, and the verdict is no longer UNKNOWN by construction

The gap §3 left open is closed, as `H1-1` §5.1 specified. `cratonvm_jit`'s
compile-time sink now carries `JDK_ONLY_VIOLATIONS_DROPPED` and exposes
`jdk_only_jit_sink_{dropped,len,saturated}`.

Two things had to change together, and the second is the interesting one:

* the violation is now built **before** the capacity test. The old order
  returned at the cap without ever asking whether the triple was already
  recorded, so the sink could not distinguish "full of other rows" from "full,
  and this row is one of them" — and therefore **could not count a distinct
  drop even if someone had added the counter**. `helpers.rs` was fixed the same
  way two days earlier;
* the cap now reads `CRATONVM_NATIVE_SHADOW_SINK_CAP` through the shared
  `resolve_capped_usize` (§1), so the knob the report ADVISES moves all three of
  its bounded collections rather than two of them.

MEASURED on a Linux build of `ab038275f`:

| `CRATONVM_NATIVE_SHADOW_SINK_CAP` | the three `cap`s | warnings |
|---|---|---:|
| unset | 4096 · 4096 · 256 | 0 |
| `9000` | 9000 · 9000 · 9000 | 0 |
| `200000` | 65536 · 65536 · 65536 | 3 |

and the report's third object is now
`"jit_compile": {"recorded": 0, "cap": 256, "truncated": false, "dropped": 0}`
— all three `truncated` lines `false`, none `null`. `census_saturation` over
that report returns **rc=0** and

```text
  saturation: none — every bounded collection reported `truncated: false`, so the
    counts above are totals, not floors.
```

**A strict run can now say "totals" and mean it.** The `UNKNOWN` branch stays in
`harness-census.sh`: every report written by an older binary still carries the
`null`, and the selftest's premise pin still holds for those.

`vm-cli` also warns on this sink like the other two, and NOTE-6 N3 is closed
with it — the saturation advice is clamped to 65,536, so the VM can no longer
print a value it would then adjust.

## 5. What this does NOT establish

* **No Windows binary contains any of §1 or §4a.** Both were built and verified
  on Linux only. The suite arms in `WORKER-5-NOTE-7` ran against prebuilt
  Windows binaries that predate them, so nothing here has been exercised by a
  105-vector sweep.
* **`jit_compile` is now measured, not exonerated.** `dropped: 0` on a
  two-thousand-put `HashMap` workload says that run did not overflow it; it says
  nothing about a real application, which is exactly the workload `G60-1` N3
  asked for and no one has run against the third sink.
* **The 70 "never native" triples were not audited individually.** The number
  is a set difference over the census; no row was traced to a registrar.
* **Both buckets are still per-suite.** A triple that is bytecode-won here could
  be native-won under a different workload. "Never native" means never in these
  105 vectors.
* **The clamp changes behaviour for anyone already passing an out-of-range
  value.** They previously got 4096 and now get 65,536 — more memory for a
  diagnostic sink, on a run that explicitly asked for more. That is the intent,
  but it is a behaviour change and not only a message.
* **The sink-cap fix was built and verified on LINUX**, against the Linux
  binary. The Windows binary used for the suite arms (`cratonvm-r10.exe`)
  predates it, so the acceptance runs in `WORKER-5-NOTE-7` do not exercise it.
* **Nothing here touches the 1402.** These are counting defects; the population
  is unchanged.

## 6. NOMINATIONS

* **N1 — re-read every `bytecode-won` figure in this directory.** It is a count
  of `(triple, kind, outcome)` rows, ~5% inflated, and — far more importantly —
  85% of the triples in it ALSO ran the native. Any record reading it as "the
  contract working" is off by ~6.9×.
* ~~**N2 — give `jit_compile`'s sink a `dropped` counter.**~~ **DONE
  2026-08-22, §4a.** The `UNKNOWN` branch stays for reports written by older
  binaries.
* ~~**N3 — `vm-cli`'s saturation advice should be clamped at the source.**~~
  **DONE 2026-08-22, §4a.** `.min(65_536)`.
* **N4 — the shared-knob pattern deserves a second look.** One knob feeding two
  sinks was already a small trap; `resolve_capped_usize` removes the drift but
  the two sinks still cannot be raised independently.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-6` — three residual instrument defects. (a)
  `CRATONVM_NATIVE_SHADOW_SINK_CAP` above the ceiling silently became the
  DEFAULT, which is smaller than the ceiling; now CLAMPED and warned, six arms
  verified on a Linux build. (b) `run.sh`'s whole-line `sort -u` counts
  `(triple, kind, outcome)`: **481 bytecode-won is 455 triples, and 385 of them
  ALSO ran native — "the contract working" is 70, a 6.9× overstatement.** (c)
  the saturation grep cannot match `"truncated": null`, which **all 105 reports
  carry**, so "totals, not floors" has been wrong on every strict run; now a
  third `UNKNOWN` verdict. MEASURED.
