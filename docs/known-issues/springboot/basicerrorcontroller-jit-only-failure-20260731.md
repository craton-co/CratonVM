# `BasicErrorControllerIntegrationTests` fails 23/26 under the JIT — and it is NOT the direct-call gate

**Status:** OPEN, measured 2026-07-31 against dev `399079cb7`.

This matters beyond the class itself: `direct_jit_callee_calls_enabled()` names
this class as its acceptance gate — *"14 consecutive clean runs, plus a
same-binary gate-closed control"*. **Neither arm is clean right now**, so anyone
re-running that gate will conclude the reopened direct-call edge is unsafe. It
is not. The failure is gate-independent.

## The matrix

| arm | result |
|---|---|
| HotSpot | **PASS 26/26**, 16.6 s |
| CratonVM `--nojit` | **PASS 26/26**, 703 s |
| CratonVM, JIT on, gate open (default) | FAIL — 26 tests, 23 failed |
| CratonVM, JIT on, `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | FAIL — 26 tests, 23 failed |

JIT-only, and unmoved by the gate. Nondeterministic: the same binary and
classpath produced `failed=23`, then `failed=15`, so **a single green run proves
nothing here**.

## Signatures

Before `7f1b1f263` (binary at `351218f44`) the class died on a receiver mix-up:

```
NoSuchMethodError method="java/lang/Class.getLog(Ljava/util/function/Supplier;)Lorg/apache/commons/logging/Log;"
                  caller="org/springframework/boot/logging/DeferredLogFactory.getLog(Ljava/lang/Class;)... @pc=12"
```

`DeferredLogFactory.getLog(Class)` calls `this.getLog(Supplier)` — an overload
on itself — and the VM took the **argument** as the receiver.

After `7f1b1f263` that signature is gone and a different one is present, so this
may be a second defect rather than the same one:

* `NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null` (×46)
* `BindException: Failed to bind properties under 'spring.main.allow-bean-definition-overriding' to boolean` (×44)
* a flood of `JIT try_patch_i32: offset out of bounds; marking buffer overflowed offset=4094 len=4094`

## The buffer overflow is worth chasing separately

`x64.rs` sizes the code buffer as `ExecutableBuffer::new(estimated_size.max(4096))`
(~line 22954). Widening the PIC inter-slot branch from `rel8` to `rel32` (part of
`7f1b1f263`) grew slot bodies, and the estimate no longer covers them.

The overflow path itself is **safe** — it sets `overflowed` and the compile
driver bails to the interpreter — so this is a silent **de-optimization**, not
corruption. But it means affected methods quietly stop being compiled at all,
which will read as an unexplained throughput loss elsewhere. It does not by
itself explain a null local, so treat it as a separate finding.

## What is ruled out

* **The Spring Boot checkout.** HotSpot passes the same classpath, and the
  failure reproduces identically against both `C:\craton\CratonVM\apps\spring-boot`
  and `CratonVM-spring-boot-residual-20260728`'s own tree.
* **The doc-23 merge.** Still fails with all four opt-outs set at once:
  `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES=1 CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE=1
  CRATONVM_STRIPED_COUNTERS_OFF=1 CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX=1`.
* **The direct-call gate**, per the matrix above.

## Window and bisect candidates

`CratonVM-spring-boot-residual-20260728`'s own suite results have this class
**PASS 26/26 (547 s) on 07-30** and **CRASH on 07-31**. Candidates in that
window: `86e5122b5` (a slot javac reuses across type categories must get no
register home), `bb26f2b65` (array receivers dispatch through Object),
`e2355534d` (a merge dropped three statements from `Op::Call`).

## Reproduction

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -ClassList <tsv> -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot" `
  -Exe <uniquely named exe> -Parallel 1 -TimeoutSec 900
```

The `-ClassList` TSV needs a literal `module<TAB>class` header row; a headerless
file silently drops its first class.
