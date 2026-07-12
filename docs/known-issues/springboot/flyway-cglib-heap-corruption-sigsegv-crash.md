# `FlywayAutoConfigurationTests` SIGSEGV — repeated heap corruption at fixed addresses (HIB-CV-32 family), crash follows a CGLIB `@Configuration` enhancement event

**Status: OPEN, characterized. Severity: HIGH (SIGSEGV crash, not just a test
failure). Likely a new occurrence of the already-tracked HIB-CV-32
heap-corruption family, not a fresh root cause.**

Found while triaging the HANG-rerun of the first Spring Boot suite run (see
[[project_spring_boot_suite_runner_20260711]]) — `FlywayAutoConfigurationTests`
(`module/spring-boot-flyway`) originally hit the 300s timeout, and at 1500s
crashes with a genuine `EXCEPTION_ACCESS_VIOLATION` (SIGSEGV) instead of
completing.

## Signature

Starting ~7s into the run and repeating roughly every 20s for the full
duration (18+ times over ~3.5 minutes before the fatal crash), the VM's own
GC guard logs **the same three heap slot addresses** as corrupted:

```
gen_heap::read_slot: corrupt Value cell (out-of-range discriminant) — returning null instead of a UB-on-match Value. Heap reference-integrity defect (see HIB-CV-32).
  slot=0x21ab2c48 raw0=0x0000000021ab2c50 raw1=0x0000000000000006
  slot=0x21ab3728 raw0=0x0000000021ab3730 raw1=0x0000000000000006
  slot=0x21ab3e40 raw0=0x0000000021ab3e48 raw1=0x0000000000000006
```
interleaved with:
```
gen_heap::get_field: out-of-bounds field read dropped (undersized object layout — class declares more fields than the object was allocated with)
  obj=0x21846b88 index=0 num_slots=0 class_id=ClassId(6) class_name=java/lang/String real_field_count=Some(4)
```
Partway through, a CGLIB `@Configuration` enhancement fires:
```
[CCE] enhance: defined org/springframework/boot/flyway/autoconfigure/FlywayAutoConfigurationTests$CustomFlywayMigrationInitializerWithJdbcConfiguration$$EnhancerByCGLIB$$0 (super=..., marker=org/springframework/context/annotation/ConfigurationClassEnhancer$EnhancedConfiguration, intercepted @Bean methods=2)
```
then the corruption warnings continue for another ~80s before the process
dies:
```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x0000000002E3007B
Faulting access: read at address 0xFFFFFFFFFFFFFFFF
Native frames: exe+0xF75E78, external/jit (x3)
```

## Analysis

- **The VM's own diagnostic explicitly names HIB-CV-32** (`gen_heap::read_slot`'s
  guard message), the already-tracked heap reference-integrity defect family
  (see memory `reference_hibcv33_nonmoving_sweep_corruptor`,
  `reference_gc_audit_stw_monitor_race_finding1`, and sibling HIB-CV-3x
  docs). This looks like a **new occurrence of that existing family**, not a
  fresh, unrelated bug — worth cross-checking against the open residuals
  there before starting a separate investigation.
- The **same three addresses** recurring stably every ~20s (not drifting)
  suggests a **periodic task re-reading one already-corrupted, long-lived
  object** (Flyway/JDBC connection-pool health check? a scheduled metrics
  poll?) rather than fresh corruption each time — the corruption event
  itself likely happened once, early, and every subsequent periodic read of
  that same object re-triggers the guard.
- The `java/lang/String` "undersized object layout" warning (`num_slots=0`
  but `real_field_count=Some(4)`) at a *different*, also-stable address
  (`0x21846b88`) is a second, possibly related symptom — an allocated
  `String` whose real 4-field layout wasn't honored at allocation time
  (0 slots allocated instead of 4). This is the same general shape as
  previously-fixed "synthetic/native wrong-layout" bugs
  (see [[reference_synthetic_native_wrong_layout_corrupts_adjacent_object]]),
  though here the object graph is entirely real bytecode (no known synthetic
  natives obviously involved) — may point at a JIT- or GC-side layout
  computation bug instead.
- The crash itself happens in **JIT-compiled code** (`external/jit` frames
  ×3), consistent with the guard's repeated "returning null instead of a
  UB-on-match Value" softening *not* fully containing the corruption —
  eventually something dereferences the bad value directly instead of going
  through the guarded read path.

## Next step

Reproduce standalone with `CRATONVM_DBG_JIT_DISASM`/`--nojit` A-B (per the
JIT/IR section of the CratonVM debug toolkit) to confirm whether disabling
JIT avoids the SIGSEGV (matching the established pattern where `--nojit`
clears a JIT-only corruption manifestation even when the underlying GC
defect is shared). Correlate the stable addresses against Flyway's own
periodic/scheduled behavior (connection validation query interval, etc.) to
identify which object is being repeatedly misread.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for module/spring-boot-flyway's FlywayAutoConfigurationTests> `
  -Start 1 -Count 1 -TimeoutSec 300 -Exe <cratonvm exe>
```
Reproduces within ~3.5 minutes; a much shorter `-TimeoutSec` than the full
1500s should still capture the early corruption warnings even if it doesn't
reach the fatal SIGSEGV.
