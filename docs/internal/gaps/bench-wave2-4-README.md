# bench/wave2-4 — `-javaagent:` end-to-end harness

WP2.4-D infrastructure for the WP2.4 work package: `Instrumentation`
interface + `-javaagent:` agent loading. Pins today's
(2026-04-25, pre-2.4-A/B/C-completion) reality of three probes so
future waves can detect drift.

## What this is

A reproducible pipeline mirroring the shape of `bench/wave2-3/`:

```
stage-instrument-probe.{sh,ps1}   ->  build hand-rolled agent.jar
stage-jacoco-probe.{sh,ps1}            (no external jar required) +
stage-mockito-probe.{sh,ps1}           locate jacocoagent.jar +
                                       locate mockito + byte-buddy +
                                       objenesis from local maven
                                       repo or vendor location.
          |
          v
run-under-cratonvm.{sh,ps1}        ->  execute each probe under
                                       target/release/cratonvm.exe with
                                       `-javaagent:`, capture stdout /
                                       stderr / rc per probe and the
                                       jacoco.exec output, summary.json
                                       rollup.
          |
          v
diff vs bench-baseline.json       ->  manual today; CI script later.
```

## The probes

| Probe | Source | External jar | Primary surface tested |
|---|---|---|---|
| `instrument` | `apps/instrument_probe/{Target,RetransformAgent,Main}.java` + `../../../apps/META-INF/MANIFEST.MF` | none — agent.jar built from these sources | `-javaagent:`, premain dispatch, `Instrumentation.addTransformer`, `retransformClasses` |
| `jacoco`     | `apps/jacoco_probe/{Target,Main}.java` | `jacocoagent.jar` (vendored at `C:/craton/ejbca-ce/lib/coverage/jacocoagent.jar` or local maven) | full JaCoCo coverage agent boot + classfile rewriter |
| `mockito`    | `apps/mockito_probe/{SomeInterface,Main}.java` | `mockito-core-*.jar` + `byte-buddy-*.jar` + `byte-buddy-agent-*.jar` + `objenesis-*.jar` | Mockito MockMaker + ByteBuddy runtime attach |

The `instrument` probe is the **primary regression test** — it has no
external dependencies and exercises the full WP2.4 path end-to-end:

1. `RetransformAgent.premain(String, Instrumentation)` is called
   before `Main.main`.
2. The agent registers a `ClassFileTransformer` that scans the raw
   classfile bytes for the 2-byte pattern `0x10 0x2a` (= `bipush 42`)
   inside the `Target` class and patches the operand byte to `0x63`
   (= 99).
3. The agent invokes `inst.retransformClasses(Target.class)` so the
   patch path fires whether `Target` was first-loaded before or after
   the transformer registered.
4. `Main.main()` calls `Target.answer()` and exits 0 only when the
   return value is 99.

## Files

| File | Purpose |
|---|---|
| `stage-instrument-probe.sh/.ps1` | javac + jar package agent.jar (Premain-Class: RetransformAgent). |
| `stage-jacoco-probe.sh/.ps1` | javac the targets; locate jacocoagent.jar. |
| `stage-mockito-probe.sh/.ps1` | javac the targets with mockito on cp; locate the four jars. |
| `run-under-cratonvm.sh/.ps1` | Run the three probes; capture per-probe `last-run-*.{stdout,stderr,rc,meta.json}` + `last-run-jacoco.jacoco.exec` + `summary.json`. |
| `bench-baseline.json` | Schema v1 — per-probe `expected_final_rc`, `expected_stderr_contains[]`, `expected_stdout_contains[]`, `known_blockers[]`. |
| `staged-instrument/`, `staged-jacoco/`, `staged-mockito/` | Output of staging step; contains `classes/`, `compile.log`, `main-class.txt`, optionally `<framework>.jar` and `skipped.flag`. |
| `last-run-*` | Outputs of running step; safe to delete. |

## Running it

### Bash (Git Bash / Linux / macOS)

```bash
# from repo root
cargo build --release -p cratonvm-cli      # once
bash bench/wave2-4/stage-instrument-probe.sh
bash bench/wave2-4/stage-jacoco-probe.sh
bash bench/wave2-4/stage-mockito-probe.sh
bash bench/wave2-4/run-under-cratonvm.sh
cat bench/wave2-4/summary.json
```

### PowerShell (Windows)

```powershell
cargo build --release -p cratonvm-cli
powershell -ExecutionPolicy Bypass -File bench\wave2-4\stage-instrument-probe.ps1
powershell -ExecutionPolicy Bypass -File bench\wave2-4\stage-jacoco-probe.ps1
powershell -ExecutionPolicy Bypass -File bench\wave2-4\stage-mockito-probe.ps1
powershell -ExecutionPolicy Bypass -File bench\wave2-4\run-under-cratonvm.ps1
type bench\wave2-4\summary.json
```

### Environment overrides

- `JAVA_HOME` — javac discovery (required if javac not on PATH).
- `JACOCO_AGENT_JAR` — full path to `jacocoagent.jar`.
- `MOCKITO_JAR`, `BYTEBUDDY_JAR`, `BYTEBUDDY_AGENT_JAR`, `OBJENESIS_JAR` — full paths.
- `CRATONVM_BIN` — override `target/release/cratonvm[.exe]`.
- `TIMEOUT_SEC` — per-probe timeout (default 60).
- `PROBE` — `instrument` | `jacoco` | `mockito` | `all` (default all).

## Expected today (2026-04-25, pre-2.4-A/B/C-completion)

| Probe | Today | After WP2.4-A/B/C land |
|---|---|---|
| `instrument` | rc=1 — premain enters, prints `agent: premain entered`, then dies on missing native `sun/instrument/InstrumentationImpl.isRetransformClassesSupported0`. | rc=0 — full transformer round-trip; stdout shows `before=99 / after=99 / OK`. |
| `jacoco`     | rc=0 — main runs; agent_loader catches JaCoCo premain NPE and continues; `jacoco.exec` is created but **empty** (transformer never installed). | rc=0 with `jacoco.exec` >300 bytes starting with magic `0x01 0xC0 0xC0`. |
| `mockito`    | rc=1 — `IllegalStateException: Could not initialize plugin: org.mockito.plugins.MockMaker` (ByteBuddy clinic fails inside `JavaDispatcher.<clinit>`). | rc=0 — `mock-class=... / default-int=0 / stubbed-int=42 / stubbed-string=hi-foo / OK`. (Also requires WP2.3.) |

The harness itself exits 0 — drift detection lives in `summary.json` +
`last-run-*.meta.json`.

## What 2.4-A / 2.4-B / 2.4-C must land for these to pass

| Sibling agent | Production change | Effect on this harness |
|---|---|---|
| 2.4-A | `vm/src/runtime/instrument.rs` registers the full `sun/instrument/InstrumentationImpl` native surface (isRetransformClassesSupported0, retransformClasses0, redefineClasses0, getAllLoadedClasses0, isModifiableClass0, getObjectSize0). | `instrument` probe makes it past the missing-native warning; `inst.isRetransformClassesSupported()` returns true; transformer registers; class is retransformed. |
| 2.4-B | `classloading/src/class_manager.rs` re-runs the transformer chain on already-loaded classes, parses the new bytes, swaps the method table in place, invalidates vtable / itable / JIT caches. | The `Target.answer() = 99` post-condition holds for the `instrument` probe; JaCoCo's per-class probe arrays get installed. |
| 2.4-C | `vm-cli/src/main.rs` + `vm/src/runtime/agent_loader.rs` — `-javaagent:` parsing + premain dispatch. **Already partially landed today** (instrument premain enters), still needs proper Instrumentation reference handed to premain. | The `agent: premain entered` line above stdout, plus all subsequent agent: lines. |

`mockito` additionally requires WP2.3 (defineClass family) — see
`bench/wave2-3/bench-baseline.json`.

## Definition of pass

`summary.json` reports rc=0 for all three probes. At that point:
- clear the per-probe `expected_stderr_contains[]` arrays in
  `bench-baseline.json`,
- flip `expected_final_rc` to 0,
- add `OK` to `expected_stdout_contains[]` for each probe,
- flip `expected_jacoco_exec_nonempty` + `expected_jacoco_magic_ok` to
  `true`.

## Anchored greps (for future WPs)

- `bench/wave2-4/stage-instrument-probe.sh` — stable, never rename.
- `bench/wave2-4/stage-jacoco-probe.sh` — stable, never rename.
- `bench/wave2-4/stage-mockito-probe.sh` — stable, never rename.
- `bench/wave2-4/bench-baseline.json` — schema stable at `$schema_version: 1`.
- `bench/wave2-4/summary.json` — `summary.probes.<probe>.rc` is the canonical drift signal.
- Probe entry points `Main` (in `apps/instrument_probe`, `apps/jacoco_probe`, `apps/mockito_probe`) are stable.
