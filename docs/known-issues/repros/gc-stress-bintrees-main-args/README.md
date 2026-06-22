# Repro set — object-binarytrees GC-frame stale-root under extreme `GC_STRESS`

See [`../../gc-stress-bintrees-object-main-args-jit-frame-stale-root.md`](../../gc-stress-bintrees-object-main-args-jit-frame-stale-root.md).

> **UPDATE (2026-06-22):** on current dev (`457c95a8`) all five repros now print
> the **correct** checksum `3222190` at `GC_STRESS=4096` (VAAload verified 8/8),
> but the GC-array-guard still fires once per run — the stale-root corruption is
> **masked, not closed** (the bad write is caught + dropped, benign at this
> depth/stress). The CRASH/WRONG results in the table below are from an *older*
> dev. The `test-infra/regression-pool/gc-root-lane.sh` GC-root acceptance lane
> records this state (status PASS, `warned>0`); see
> [`../../../feature-designs/precise-jit-maps-default.md`](../../../feature-designs/precise-jit-maps-default.md)
> "Step 4 baseline".

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
