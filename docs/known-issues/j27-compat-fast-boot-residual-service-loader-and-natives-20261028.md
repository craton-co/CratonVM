# `--compatible` fast boot: ServiceLoader, CLDR, `VM.savedProps` and `NativeSeedGenerator` failures remain

> **rx27 wave 3 (lane compat, 2026-10-28): Linux rows FIXED, VERIFIED AGAINST A BINARY 2026-10-28.** `VM.savedProps` (CLDR, `ThreadLocalRandom`), ServiceLoader "during VM init" and the `jdk.unsupported` opens are fixed (savedProps port, boot layer, level 4). A Linux check with `--add-opens java.base/java.lang.invoke=ALL-UNNAMED` (`MethodHandle.form`), `sun.misc.Unsafe.theUnsafe` and `SecureRandom.getSeed` equals HotSpot in both modes. Not re-measured: the Windows `NativeSeedGenerator.nativeGenerateSeed` row (the native prune that dropped `sun/*` natives was removed in rx27 wave 2, the likely cause); retire once a Windows run confirms. Host, binary `cvm-compat-c2` (branch `rx27-compat` = `jit-regex-r27` dc5f532718 + this lane), facts battery against HotSpot 25 (`hs2`): `--compatible` default 83 of 89 probes equal (`cmp2`; 0 on b0/b2, 59 after the wave-2 prune fix), `--compatible` with `CRATONVM_COMPAT_FAST_BOOT_JIT=0` 83 of 89 (`cmp2f`; every probe silent before). The 6 left differ in `--jdk-only` too (HotSpot `OmitStackTraceInFastThrow` counters and messages, an ICCE frame, identity-hash text): `jit/rx27-compat-battery-rows-that-differ-in-both-modes-20261028.md`.

Status: OPEN (found by lane WINPOLL, j27 wave 4, 2026-10-28; Windows)
Area: VM boot, `--compatible` fast boot (`CRATONVM_COMPAT_FAST_BOOT_JIT`); follow-up to
`docs/internal/fixed-bugs/j27-compat-fast-boot-jit-runs-no-main-20261028-FIXED-20261029.md` (output and hello-world are fixed)

## Evidence

Windows, JDK 25.0.3, release builds of the j27 tree at 12:16 and 13:49 on 2026-10-28
(`C:\craton\j27-winpoll-bin\cvm-winpoll-{1,2}.exe`). Probes from `tools/probes/jit/{j27,r22}` plus a GC/thread set,
run with `--compatible --enable-native-access=ALL-UNNAMED --add-opens java.base/java.lang.invoke=ALL-UNNAMED`.

- Hello-world prints with fast boot on. The silent-output defect is gone.
- With fast boot on (the default), 12 probes fail (rc=1). With `CRATONVM_COMPAT_FAST_BOOT_JIT=0`, every one of them
  passes, and the `--compatible` facts then equal the `--jdk-only` facts. The failures are the same with
  `CRATONVM_JIT_PAGE_POLL` on or off, and under G1, ZGC and Serial.

| probe | first error with fast boot on |
|---|---|
| R21MonitorRows, R22InlineReflectLoop | `ExceptionInInitializerError` caused by `IllegalStateException: Not yet initialized` at `jdk.internal.misc.VM.getSavedProperty` from `ThreadLocalRandom.<clinit>` |
| R23VolatileSb, T1W8StaticfwdSyncRows | `ServiceConfigurationError: Locale provider adapter "CLDR"cannot be instantiated` |
| R25ThreadDumpPauses, R25LockAttribution, R25WaitingSampleShape | `ServiceConfigurationError: sun.management.spi.PlatformMBeanProvider: not accessible to module java.management during VM init` |
| R22SyncOpenOptions | `UnsatisfiedLinkError: 'boolean sun.security.provider.NativeSeedGenerator.nativeGenerateSeed(byte[])'` |
| R22InlineMhCustomize, R22RuntimeCtorCustomize, R22W4RuntimeEdges | `InaccessibleObjectException` for `MethodHandle.form`: the `--add-opens java.base/java.lang.invoke=ALL-UNNAMED` flag is not applied |
| R22W4SyncEdges | `InaccessibleObjectException` for `sun.misc.Unsafe.theUnsafe` (`jdk.unsupported` does not open `sun.misc`) |

The facts above suggest that the fast arm still skips part of HotSpot's `initPhase2` / `initPhase3`. The symptoms are:
- module-layer opens and `--add-opens` are not applied;
- the VM init level stays below "booted" while ServiceLoader runs;
- the remaining `VM.savedProps` readers fail.

Fast boot also does not register the Windows `NativeSeedGenerator` native.

## Reproduce

```
cd C:\craton\j27-winpoll-bin\probe
set JAVA_HOME=C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot
..\cvm-winpoll-2.exe --compatible -cp cls R25ThreadDumpPauses        (fails)
set CRATONVM_COMPAT_FAST_BOOT_JIT=0
..\cvm-winpoll-2.exe --compatible -cp cls R25ThreadDumpPauses        (R25-PASS, as on HotSpot)
```

## What would fix it

The fast-boot arm of `Vm::boot_init_phase1` (vm/src/vm/vm_init.rs) has to own everything HotSpot's `initPhase2`
(module system, including `--add-opens` / `--add-exports`) and `initPhase3` (`VM.initLevel` up to `SYSTEM_BOOTED`,
security and seed natives) establish before user `main`. Each row above is a regression test: run it under fast
boot, then under `=0`, and compare both runs with HotSpot.
