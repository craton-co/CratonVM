# Repro set — object-binarytrees GC-frame stale-root under extreme `GC_STRESS`

Writeup (FIXED): [`docs/internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`](../../app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md).

> **✅ FIXED (2026-06-22, dev `77c98761`).** Root cause: the compiled entry-point
> `main`'s JIT frame is invisible to `gc_quiescence` (invoked via `Vm::invoke`
> without a `JitEntryGuard`), so the **moving** young collector relocated its
> roots and couldn't rewrite the raw stack slots → stale all-zero receiver. Fix:
> detect an unregistered JIT frame on the native stack → non-moving sweep +
> full-stack mark. **`VAAload 14 @GC_STRESS=4096` now prints `3222190` cleanly
> (5/5), no guard warnings.** The CRASH/WRONG/“masked” notes in the table below
> are from *older* dev and are superseded. These repros remain the regression
> fixtures for `../../../../../test-infra/regression-pool/gc-root-lane.sh`.

All build with JDK 25 `javac`. Run under the release `cratonvm.exe`.

| file | what | `CRATONVM_DBG_GC_STRESS=4096` result |
|------|------|--------------------------------------|
| `binarytrees.java` | canonical object-based binarytrees (accumulating single checksum), `maxDepth` from `args[0]` | **CRASH / empty** (want `3222190` for arg `14`) |
| `VAAload.java` | minimal: `main` reads `args[0]` (`int maxDepth = args[0].length()+12`) | **CRASH 8/8** (set_field AIOOBE) |
| `VArgLen.java` | minimal: `main` reads `args.length` (`int maxDepth = args.length+13`) | **WRONG OUTPUT** (deterministic `1348958`, want `3222190`) |
| `RHard.java` | identical to binarytrees but `int maxDepth = 14` (hard-coded, `args` untouched) | **CLEAN 8/8** |
| `VStatic.java` | `int maxDepth = seed` (a non-constant `static int`, `args` untouched) | **CLEAN 8/8** |

The single discriminator between the crashing and clean variants is **whether `main`
loads its `args` parameter** (`aload_0`, a reference in local 0). `VStatic` proves it
is not merely "`maxDepth` is non-constant"; `RHard` proves the rest of the structure is
innocent.

```bash
javac -d . VAAload.java
for i in $(seq 1 8); do CRATONVM_DBG_GC_STRESS=4096 cratonvm.exe -Xmx6g -cp . VAAload 14; done
# want 3222190 every run; observe empty (crash) instead
```

`bt14` checksums: bt10=`135854` bt14=`3222190` bt16=`14985902` bt18=`68332206`.
