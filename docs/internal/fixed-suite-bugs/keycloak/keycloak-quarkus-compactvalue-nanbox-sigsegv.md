# Keycloak quarkus/runtime CompactValue NaN-box SIGSEGV

Status: fixed on current `dev` before this branch; validated 2026-07-04 while taking on `keycloak-07-04` bugs.

## Validation

`quarkus/runtime :: org.keycloak.quarkus.runtime.cli.PicocliTest` no longer exits with `rc=139` after the compact-long local-kind fix present on `dev`. The focused run with `cratonvm-keycloak-0704-172634` reached a 120s timeout instead of the raw native crash.

The post-crash PicocliTest timeout residual was fixed later; see `quarkus-runtime-picocli-post-compactvalue-hang.md`.

---

# quarkus/runtime: silent SIGSEGV after CompactValue NaN-box collision

Historical original status: open - genuine VM-level crash, highest severity of this sweep. Kept for provenance; current dev no longer reproduces the raw crash.

Date observed: 2026-07-04

## Summary

4 classes in `quarkus/runtime` crash the CratonVM process (rc=139, killed by
SIGSEGV) with **no exception, no panic message, no Java stack trace** — the
log simply stops mid-stream:

```
DEBUG [io.netty.util.internal.PlatformDependent] -Dio.netty.noPreferDirect: false
DEBUG [io.netty.util.NetUtil] /proc/sys/net/core/somaxconn: 4096
CompactValue: first long<->object NaN-box collision degraded to Value::Long
(SUB_OBJECT-patterned primitive long reached a context-free decoder).
Subsequent collisions are counted by object_degradation_count() but not logged.
```
(log ends here — process exit code 139)

Unlike every other crash found in this sweep, this is **not** caught by any
of CratonVM's defensive guards (the OOB field-read/write guard, the
LinkageError-wrapping NoSuchMethodError path, etc.) — it's a raw segfault.
The last line printed is itself a CratonVM diagnostic about a NaN-boxing
representation collision being "degraded" (handled, per the message), so the
actual fault happens somewhere shortly after that degradation path, not
inside it necessarily.

## Scale

4/4 classes in `quarkus/runtime`, all crash identically:
- `cli.PicocliTest`
- `cli.UpdateCompatibilityPicocliTest`
- `configuration.ConfigurationTest`
- `configuration.DatasourcesConfigurationTest`

## Why this is high priority

Every other crash bucket in this sweep is either a caught/thrown Java
exception (`NoSuchMethodError`, `AbstractMethodError`, `class file error`) or
a guarded-and-dropped bad memory access (the `gen_heap::get_field`/`set_field`
"out-of-bounds ... dropped" warnings). This is the only **uncaught native
crash** found — the "NaN-box collision degraded to Value::Long" message
implies CratonVM's compact tagged-value representation hit a representation
ambiguity between a boxed `long` and an object pointer, and while the
degradation path itself logs that it handled the immediate case, something
downstream doesn't tolerate the degraded representation and dereferences
something invalid.

## Next steps

1. Get a symbolicated native backtrace: rebuild with
   `[profile.release] strip = "none"` (temporarily, per
   `reference_worktree_build_recipe`) or run under `gdb`/`rr` on the Azure
   Linux host to catch the actual SIGSEGV instruction pointer, since the
   Rust-level log gives no panic/backtrace to work from.
2. Search the codebase for the exact log site ("NaN-box collision degraded to
   Value::Long") to find the CompactValue decoder code path involved, and
   what "SUB_OBJECT-patterned primitive long" means in context — that should
   narrow which value/field is ambiguous.
3. Isolate which of the 4 classes' setup is minimal enough for a standalone
   repro (Picocli CLI parsing + Quarkus runtime `ConfigurationTest`/
   `DatasourcesConfigurationTest` — likely something in common between them,
   given identical crash signature across CLI and config-test code).

## Repro

```
ssh victor@20.84.156.31   # Azure build host
cd /data/wt-keycloak-full-20260704
apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\nquarkus/runtime\torg.keycloak.quarkus.runtime.cli.PicocliTest\n') \
  -TimeoutSec 60 -RunName repro-nanbox-sigsegv \
  -Exe target/release/cratonvm-kcfull1124 -JdkHome /home/victor/jdk25
```

## Evidence

`/data/wt-keycloak-full-20260704/apps/keycloak-suite-runner/.suite/results/kcfull-others-1124-20260704-v2/others-jit/logs/quarkus_runtime.org.keycloak.quarkus.runtime.cli.PicocliTest.{out,err}.log` and the 3 sibling class logs (identical tail).
