# bench/wave2-3 — CGLIB + ByteBuddy acceptance harness

WP2.3-D infrastructure. Pins today's (2026-04-25, pre-2.3-A/B/C) reality of
CGLIB's `Enhancer.create()` and ByteBuddy's `new ByteBuddy().subclass(...)`
under cratonvm so future waves can detect drift.

## What this is

A reproducible pipeline mirroring the shape of `bench/wildfly/`:

```
stage-cglib-probe.{sh,ps1}       ->  locate cglib-nodep jar (or skip),
stage-bytebuddy-probe.{sh,ps1}        compile probes into staged-*/classes
          |
          v
run-under-cratonvm.{sh,ps1}        ->  execute each probe under
                                      target/release/cratonvm[.exe], capture
                                      stdout / stderr / rc per probe,
                                      summary.json rollup
          |
          v
diff vs bench-baseline.json       ->  manual today; CI script later
```

The probes intentionally come in **two shapes per framework**:

| Framework | Real-DSL probe (uses jar) | Synthetic fallback (no jar) |
|---|---|---|
| CGLIB | `apps/cglib_probe/CglibProbe2.java` | `apps/cglib_probe/CglibProbe.java` |
| ByteBuddy | `apps/bytebuddy_probe/ByteBuddyProbe.java` | `apps/bytebuddy_probe/ByteBuddyProbeSynth.java` |

The real-DSL probes satisfy the spec verbatim (acceptance text in
`docs/wildfly-ejbca-roadmap.md` §5 WP2.3). The synthetic probes feed
pre-compiled subclass bytes through `ClassLoader.defineClass` directly —
the same WP2.3 entry point at the bottom of CGLIB / ByteBuddy's loader
chain — and let the harness exit cleanly when no real jar is available.

## Files

| File | Purpose |
|---|---|
| `stage-cglib-probe.sh/.ps1` | Locate cglib jar; compile probes into `staged-cglib/classes/`. |
| `stage-bytebuddy-probe.sh/.ps1` | Locate byte-buddy jar; compile probes into `staged-bytebuddy/classes/`. |
| `run-under-cratonvm.sh/.ps1` | Run both probes; capture per-probe `last-run-<probe>.{stdout,stderr,rc,meta.json}` and `summary.json`. |
| `bench-baseline.json` | Schema v1 — per-probe `expected_final_rc`, `expected_stderr_contains[]`, `expected_stdout_contains[]`, `known_blockers[]`. |
| `staged-cglib/`, `staged-bytebuddy/` | Output of staging step; contains `classes/`, `compile.log`, `main-class.txt`, optionally `<framework>.jar` and `skipped.flag`. |
| `last-run-*` | Outputs of running step; safe to delete. |

## Running it

### Bash (Git Bash / Linux / macOS)
```bash
# from repo root
cargo build --release -p cratonvm-cli      # once
bash bench/wave2-3/stage-cglib-probe.sh
bash bench/wave2-3/stage-bytebuddy-probe.sh
bash bench/wave2-3/run-under-cratonvm.sh
cat bench/wave2-3/summary.json
```

### PowerShell (Windows)
```powershell
cargo build --release -p cratonvm-cli
powershell -ExecutionPolicy Bypass -File bench\wave2-3\stage-cglib-probe.ps1
powershell -ExecutionPolicy Bypass -File bench\wave2-3\stage-bytebuddy-probe.ps1
powershell -ExecutionPolicy Bypass -File bench\wave2-3\run-under-cratonvm.ps1
type bench\wave2-3\summary.json
```

### Environment overrides
- `JAVA_HOME` — javac discovery (required if javac not on PATH).
- `CGLIB_JAR` — full path to `cglib-nodep-X.Y.jar` to skip auto-discovery.
- `BYTEBUDDY_JAR` — full path to `byte-buddy-X.Y.Z.jar`.
- `CRATONVM_BIN` — override `target/release/cratonvm[.exe]`.
- `TIMEOUT_SEC` — per-probe timeout (default 60).
- `PROBE` — `cglib` | `bytebuddy` | `both` (default both).

## Expected today (2026-04-25, pre-2.3-A/B/C)

- `stage-cglib-probe`: rc=0; locates `C:/Users/<dev>/.m2/repository/cglib/cglib-nodep/2.2/cglib-nodep-2.2.jar` on the dev workstation; compiles `CglibProbe2`.
- `stage-bytebuddy-probe`: rc=0; locates `C:/Users/<dev>/.m2/repository/net/bytebuddy/byte-buddy/1.14.19/byte-buddy-1.14.19.jar`; compiles `ByteBuddyProbe`.
- `run-under-cratonvm`: each probe rc=1.
  - CGLIB synthetic mode: `NullPointerException: Cannot invoke getCodeSource on null` — the post-defineClass `getProtectionDomain()` chain returns `null`. WP2.3-C must add ProtectionDomain.
  - CGLIB real mode: deeper inside `Enhancer.create()`, CGLIB hits `Unsafe.defineClass` / `MethodHandles.Lookup.defineClass`, both stubs today (WP2.3-A + 2.3-B).
  - ByteBuddy real/synthetic: the same family of failures; ByteBuddy reaches reflection paths (WP2.1) before reaching `defineClass`.
- The harness itself exits 0 — drift detection lives in `summary.json`.

## What 2.3-A/B/C must land for these to pass

| Sibling agent | Production change | Effect on this harness |
|---|---|---|
| 2.3-A | `classloading/src/class_manager.rs::define_class_with_options` accepts a bytes blob, parses, links, registers a `Class` with a `ProtectionDomain` and a synthesised `CodeSource`. | CGLIB/ByteBuddy can produce a real `Class<?>` post-DSL. |
| 2.3-B | `native-builtins/src/unsafe_natives.rs::defineClass` routes to A's API (currently throws / stubs). | Real ByteBuddy's class loading strategy lands successfully. |
| 2.3-C | `native-builtins/src/classloader.rs::ClassLoader.defineClass*` family (incl. `defineClass1`/`defineClass2`) routes into A's API; populates the new class's `ProtectionDomain`. | Synthetic CglibProbe stops NPE-ing on `getCodeSource`; both real probes complete. |

## Definition of pass

`summary.json` reports rc=0 for both probes with `main_class` = `CglibProbe2`
and `ByteBuddyProbe` (i.e. real-DSL paths exercised). At that point clear
the per-probe `expected_stderr_contains[]` lists in `bench-baseline.json`,
flip `expected_final_rc` to 0, and add `OK` to `expected_stdout_contains[]`
in the same PR that ships the fix.

## Anchored greps (for future WPs)

- `bench/wave2-3/stage-cglib-probe.sh` — stable, never rename.
- `bench/wave2-3/stage-bytebuddy-probe.sh` — stable, never rename.
- `bench/wave2-3/bench-baseline.json` — schema stable at `$schema_version: 1`.
- `bench/wave2-3/summary.json` — `summary.probes.<probe>.rc` is the canonical drift signal.
- Probe entry points `CglibProbe`, `CglibProbe2`, `ByteBuddyProbe`, `ByteBuddyProbeSynth` are stable.
