# JDK-only mode — benchmarks and non-regression budgets

| | |
|---|---|
| **Status** | **No baseline has been captured.** Every number in the budget table is a *proposed engineering gate*, not a measurement. |
| **Normative source** | [`../feature-designs/jdk-only-mode.md`](../feature-designs/jdk-only-mode.md) |
| **Methodology** | [`../../BENCHMARK.md`](../../BENCHMARK.md) is the house standard and takes precedence on anything this file does not cover. |
| **Related** | [`known-issues/jdk-only/runtime-services-blocker-inventory.md`](../known-issues/jdk-only/runtime-services-blocker-inventory.md) (the open blocker inventory) |

## Why strict mode needs its own benchmark plan

`--jdk-only` moves work in both directions and the net is not obvious:

- **Cost.** More JDK bytecode is interpreted where a native previously answered
  directly. Until the JIT covers the newly-hot JDK methods, this shows up as
  startup and steady-state regression.
- **Saving.** Thousands of registrations disappear from the native registry, and
  dispatch does less lookup. This shows up as faster initialisation and cheaper
  invocation.

Because they pull opposite ways, a single wall-clock number hides the mechanism.
Measure class loading, native lookup, interpreter execution and JIT steady state
**separately**, or you will not be able to attribute a regression.

The house rule from [`../../BENCHMARK.md`](../../BENCHMARK.md) applies without
exception: **checksums must match HotSpot on every run.** A fast wrong answer is
a bug, not a result. Every harness in `bench/` prints its time *and* its
checksum for this reason.

---

## Metric table

| # | Metric | Method | Comparison |
|--:|---|---|---|
| 1 | **Startup wall time** | Median of ≥ 20 process launches of a trivial main | `--jdk-only` vs `--real-jdk` |
| 2 | **Peak RSS / private bytes** | `/usr/bin/time -v` on Linux; process metrics on Windows | Both modes and HotSpot |
| 3 | **Loaded class count** | `--dump-class-origins`, counted per origin | Real class count vs generated-class count |
| 4 | **Native registry size** | `--dump-native-registry` → `.counts` | `bridge` / `intrinsic` / `synthetic-stub` / `total` |
| 5 | **Native dispatches per kind** | Per-kind invocation counters (`NativeMethodRegistry::invocations_of_kind`) | Detects accidental native takeover; strict `synthetic-stub` must be 0 |
| 6 | **Bytecode instruction count** | Interpreter counters | Quantifies work moved out of native code — the *mechanism* behind metrics 1 and 8 |
| 7 | **JIT compilation count and time** | Existing JIT diagnostics | Identifies newly-hot JDK methods, which is where strict-mode tuning goes |
| 8 | **Core corpus throughput** | `bench/CratonBench.java` phases + the per-kernel harnesses | Strict vs compatible vs HotSpot |
| 9 | **GC pause and root count** | Existing GC diagnostics | Especially the concurrent-worker vectors, where real worker threads add real roots |
| 10 | **Binary size** | Release artifacts | Default build, `--features synthetic-jdk`, and any future hardened build |

Metrics 3, 4 and 5 are **free** — they fall out of dumps the audit already
collects. Capture them on every benchmark run; they are what explains a move in
metrics 1, 6 and 8.

---

## Measurement commands

### Setup

```bash
export JAVA_HOME=/path/to/jdk25

cargo build --release -p cratonvm-cli

mkdir -p target/jdk-only-tests/25
"$JAVA_HOME/bin/javac" --release 25 -d target/jdk-only-tests/25 bench/*.java

git rev-parse HEAD > target/jdk-only-bench.commit
```

Record host, OS, CPU model, core count, JDK vendor/version, and the commit with
every result set. A number without that metadata is not comparable to anything.

### 1–2. Startup and peak memory (Linux)

```bash
for mode in "--real-jdk" "--jdk-only"; do
  echo "== $mode =="
  for i in $(seq 1 20); do
    /usr/bin/time -f '%e %M' \
      target/release/cratonvm \
      $mode \
      --java-home "$JAVA_HOME" \
      -cp vm-cli/tests/resources \
      HelloWorld \
      >/dev/null
  done
done
```

Report the **median**, not the mean — one scheduler hiccup ruins a mean over 20
samples. Report the interquartile range alongside it.

On Windows, `/usr/bin/time` does not exist; use
`Measure-Command` for wall time and `Get-Process`/`Win32_Process` peak working
set for memory, or run this metric on Linux only and say so.

### 3–4. Class origins and registry size

```bash
mkdir -p target/jdk-only-bench

target/release/cratonvm --jdk-only --java-home "$JAVA_HOME" \
  --dump-class-origins   target/jdk-only-bench/classes-strict.json \
  --dump-native-registry target/jdk-only-bench/registry-strict.json \
  -cp vm-cli/tests/resources HelloWorld

# Class count by origin.
jq -r '.classes | group_by(.origin) | map({(.[0].origin): length}) | add' \
  target/jdk-only-bench/classes-strict.json

# Registry composition. Works against today's dump shape (see
# jdk-only-audit.md §3.3) — there is no schema_version key yet.
jq '.counts' target/jdk-only-bench/registry-strict.json
```

Compare against the `--real-jdk` run of the same program. The interesting
quantity is the *delta*: how many classes moved from `compatibility-stub` to
`boot-image`, and how many registrations vanished.

### 5. Native dispatches per kind

```bash
jq -r '
  .natives
  | group_by(.kind)
  | map({kind: .[0].kind,
         entries: length,
         invocations: (map(.invocations // 0) | add)})
' target/jdk-only-bench/registry-strict.json
```

`invocations` requires the schema-v2 census field; the `// 0` fallback keeps the
query runnable against today's dump. The strict-mode assertion is absolute:
`synthetic-stub` invocations must be **zero**, regardless of what it costs.

### 8. Corpus throughput

```bash
for mode in "--real-jdk" "--jdk-only"; do
  for phase in arithmetic fib sieve matrix hashmap stringregex bintrees; do
    target/release/cratonvm $mode --java-home "$JAVA_HOME" \
      -cp target/jdk-only-tests/25 CratonBench "$phase"
  done
done
```

Each phase prints its time **and** its checksum. A phase whose checksum differs
between modes is a correctness failure and its timing is void — do not record
it as a slow result. The known invariant checksums are tabulated in
[`../../BENCHMARK.md`](../../BENCHMARK.md) (e.g. BinTrees `68332206` at depth 18,
HashMap `15499991500000` at n = 1,000,000).

Compare **before/after on one host with one binary pair**. On shared build hosts
that is often the only trustworthy comparison available.

### 9–10. GC and binary size

```bash
# GC pause / root counts come from the existing GC diagnostics; run the
# concurrent vector explicitly since it is not in the suite's default set.
CV="$PWD/target/release/cratonvm" JDK="$JAVA_HOME" \
  ONLY="RConcurrent" bash regression-suite/run.sh

ls -l target/release/cratonvm
```

`RConcurrent` is deliberately excluded from `regression-suite/run.sh`'s default
class list because it intermittently trips a documented cross-thread JIT-frame
root-scanning gap. Under strict mode, real ForkJoin/executor worker threads
execute real bytecode and add real roots, so this vector is *more* stressed, not
less. Treat flakes here as data about metric 9, not as noise to retry away.

---

## Proposed non-regression budgets

> **These are proposed engineering gates, not measurements, and not a
> commitment.** No baseline exists yet. Recalibrate every row after the first
> clean baseline on a fixed host, and record the recalibration date here.

| Budget | Proposed limit | Applies to |
|---|---|---|
| Startup wall time | ≤ **+20 %** vs `--real-jdk` | First experimental release |
| Peak memory | ≤ **+15 %** vs `--real-jdk` | Core corpus |
| Steady-state throughput | ≤ **−10 %** after JIT warm-up | Collections and strings |
| Thread / class / registry growth | **no unbounded growth** | Any long-running vector |
| Synthetic-stub invocations | **exactly 0** | All strict runs, unconditionally |

The first four are negotiable against evidence. The fifth is not a performance
budget at all — it is the feature's defining invariant, and it holds regardless
of what it costs.

### Recalibration procedure

1. Pick one host. Record its full metadata.
2. Build both configurations from **one** commit.
3. Capture all ten metrics in both modes.
4. Commit the raw numbers, with host metadata and commit SHA, before proposing
   revised limits.
5. Update the table above and date the change.

Do not recalibrate against a run whose checksums did not match HotSpot, and do
not recalibrate against numbers gathered on a machine that was doing something
else at the time.

---

## Interpreting a regression

| Observation | Likely mechanism | Where to look |
|---|---|---|
| Startup up, class count up, registry down | Expected: real classes are loading where stubs used to short-circuit them. | Metrics 3 + 4 together |
| Startup up, class count **flat** | Not the expected mechanism. Suspect dispatch overhead or a lookup path that got slower. | Metrics 5 + 6 |
| Throughput down, bytecode instruction count up sharply | Work genuinely moved from native to interpreter. | Metric 7 — the newly-hot methods are the JIT's next targets |
| Throughput down, instruction count flat | Not a work-volume change. Suspect dispatch, inline-cache behaviour, or a lost intrinsic. | Metric 5, then the resolver |
| Memory up, class count up | More real classes means more metadata. Usually proportional. | Metric 3 |
| Memory up, class count flat | Suspect a leak, a retained side table, or a fabricated-layout structure that outlived its purpose. | GC diagnostics |
| GC pause up on concurrent vectors | Real worker threads executing real bytecode add real roots. | Metric 9 |

The general rule: **profile before calling it an interpreter-throughput wall.**
More than one apparent "interpreter is slow" result in this repository's history
turned out to be a specific dispatch or cache defect that a profile named in
minutes.
