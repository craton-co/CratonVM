# `BasicErrorControllerIntegrationTests` is NOT a usable acceptance gate right now

**Status: FIXED — 2026-08-01. The title is no longer true; the class IS a
usable gate again.** Retired from `docs/known-issues/springboot/`.

This document was right about the important thing: the failure it recorded was
neither the direct-call gate nor the `checkcast` abort its two companions
described, and anyone running
`jit/src/lib.rs::direct_jit_callee_calls_enabled()`'s stated acceptance would
have concluded the reopened JIT-to-JIT edge was unsafe when it was not.

Its two signatures —

```
NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local5>" is null
BindException: Failed to bind properties under 'spring.main.allow-bean-definition-overriding' to boolean
```

— were one defect: the JIT-to-JIT handler-resume sink seeding a handler frame
with the callee's incoming arguments only, for a method the precise-handler-frame
relaxation now admits. Fixed in `843b780baa`; full write-up in
[`basicerrorcontroller-class-cluster-20260728-FIXED.md`](basicerrorcontroller-class-cluster-20260728-FIXED.md).

Two items from this document did NOT close with it:

* The `DeferredLogFactory.getLog(Class)` receiver mix-up recorded under
  "Signature observed here" was last seen on a binary at `351218f44` and has
  not been observed since `7f1b1f263`. It was never root-caused. If it returns,
  it is a separate defect — do not assume the handler-frame fix covers it.
* The code-buffer overflow flood is re-filed, with a corrected attribution
  (it is the optimizing IR tier's estimate, not `x64.rs`'s), as
  `docs/known-issues/jit-ir-tier-code-buffer-overflow-flood-20260801.md`.

---

*(The original report follows in full. Its own `Status:` line records the 2026-07-31 state and is superseded by the header above.)*

# `BasicErrorControllerIntegrationTests` is NOT a usable acceptance gate right now

**Status:** OPEN, measured 2026-07-31 against dev `f14b64379`; still present
2026-08-01 (26 tests, 23 failed, JIT on).

This is a narrow companion to the two other reports on this class —
[`../../internal/fixed-suite-bugs/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md`](../../internal/fixed-suite-bugs/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731-FIXED.md)
(**FIXED 2026-08-01**, retired to the internal folder) and
[`basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md`](basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md).
It does not re-file those. It records one fact they do not cover, which will
otherwise cause a wrong conclusion about a different subsystem.

**This is now the only thing keeping the class red.** The checkcast abort is
closed. What remains is `IllegalStateException: Cannot bind to
SpringApplication` -> `BindException` -> `NullPointerException` in
`BindConverter.convert` (`Cannot invoke "java.util.Iterator.hasNext()" because
"<local5>" is null`), which is JIT-only and unrelated to the
collection-overlay GC mechanism that produced the abort.

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
