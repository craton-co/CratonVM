# `BasicErrorControllerIntegrationTests` is NOT a usable acceptance gate right now

**Status:** OPEN, measured 2026-07-31 against dev `f14b64379`.

This is a narrow companion to the two existing reports on this class —
[`../spring/springboot-basicerrorcontroller-checkcast-abort-20260731.md`](../spring/springboot-basicerrorcontroller-checkcast-abort-20260731.md)
and
[`../spring/basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md`](../spring/basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md).
It does not re-file those. It records one fact they do not cover, which will
otherwise cause a wrong conclusion about a different subsystem.

## Why this needs saying

`jit/src/lib.rs::direct_jit_callee_calls_enabled()` names this class as the
acceptance gate for the reopened raw JIT-to-JIT edge:

> Acceptance: `BasicErrorControllerIntegrationTests`, default flags, 14
> consecutive clean runs, plus a same-binary gate-closed control.

**Neither arm is clean today**, so anyone running that gate will read the
result as the reopened direct-call edge being unsafe. It is not. The failure is
independent of the gate:

| arm | result |
|---|---|
| HotSpot | **PASS 26/26**, 16.6 s |
| CratonVM `--nojit` | **PASS 26/26**, 703 s |
| CratonVM, JIT on, gate open (default) | FAIL — 26 tests, 23 failed |
| CratonVM, JIT on, `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | FAIL — 26 tests, 23 failed |

JIT-only, and unmoved by the gate. Also **nondeterministic** — the same binary
and classpath gave `failed=23`, then `failed=15` — so a single clean run of
this class proves nothing, for this gate or any other.

Until this class is green again, validate that edge with
`probes/CallFloorProbe.java` plus a suite that currently passes, and treat the
comment's acceptance line as unrunnable rather than as failing.

## Signature observed here

Distinct from the `checkcast: not an object reference` hard abort in the
companion docs — this arm does not abort, it fails assertions:

* `NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null` (×46)
* `BindException: Failed to bind properties under 'spring.main.allow-bean-definition-overriding' to boolean` (×44)

On a binary at `351218f44` (before `7f1b1f263`) the same class instead died on
a receiver mix-up, which is worth recording since it may be a separate defect:

```
NoSuchMethodError method="java/lang/Class.getLog(Ljava/util/function/Supplier;)Lorg/apache/commons/logging/Log;"
                  caller="org/springframework/boot/logging/DeferredLogFactory.getLog(Ljava/lang/Class;)... @pc=12"
```

`DeferredLogFactory.getLog(Class)` calls `this.getLog(Supplier)` — an overload
on itself — and the VM took the **argument** as the receiver.

## Separate finding: the code buffer now overflows

The failing runs emit a flood of:

```
JIT try_patch_i32: offset out of bounds; marking buffer overflowed offset=4094 len=4094
```

`x64.rs` sizes the buffer as `ExecutableBuffer::new(estimated_size.max(4096))`
(~line 22954). Widening the PIC inter-slot branch from `rel8` to `rel32` (part
of `7f1b1f263`) grew slot bodies past that estimate.

The overflow path is **safe** — it sets `overflowed` and the compile driver
bails to the interpreter — so this is a silent **de-optimization**, not
corruption. But affected methods quietly stop being compiled, which will later
read as an unexplained throughput loss. It does not by itself explain a null
local, so treat it as its own item.

## Ruled out

* **The Spring Boot checkout.** HotSpot passes the same classpath, and the
  failure reproduces identically against both `C:\craton\CratonVM\apps\spring-boot`
  and `CratonVM-spring-boot-residual-20260728`'s own tree.
* **The doc-23 merge.** Still fails with all four opt-outs set at once:
  `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES=1 CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE=1
  CRATONVM_STRIPED_COUNTERS_OFF=1 CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX=1`.

## Reproduction

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -ClassList <tsv> -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -JdkHome "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot" `
  -Exe <uniquely named exe> -Parallel 1 -TimeoutSec 900
```

The `-ClassList` TSV needs a literal `module<TAB>class` header row; a headerless
file silently drops its first class.
