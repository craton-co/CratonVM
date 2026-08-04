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
  `docs/internal/fixed-suite-bugs/jit-ir-tier-code-buffer-estimate-20260801-FIXED.md` (FIXED 2026-08-01: the estimate was the IR tier's and is now fitted to a 1664-compile census).

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

---

## Addendum, 2026-08-01: the gate was run, and item 3 has a probe

Added by `fix/basicerrorcontroller-jit-20260801`, which reached the same
handler-frame diagnosis independently from this class while the fix above was
landing from `LiquibaseAutoConfigurationTests` and devtools. Its implementation
was dropped in favour of `843b780baa` — that one also repairs slot 0 from the
caller's arguments, which this one did not and which matters, since the reason-9
snapshot records `this` as `Undefined` whenever liveness says the bytecode has
no further read of it (exactly what the real `BindConverter.convert` frame does:
`getfield delegates` at bci 3 is its last use). What it contributes instead:

### The acceptance gate, run and MET

This report existed because
`jit/src/lib.rs::direct_jit_callee_calls_enabled()`'s stated acceptance was
unrunnable. It has now been run, on one binary, Linux x86-64, real JDK 25,
Spring Boot 4.1.0-SNAPSHOT, 3 concurrent:

| arm | runs | clean |
|---|---|---|
| default flags | 14 | **14 consecutive** |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` (gate-closed control) | 2 | **2** |

Both arms clean is what settles the question the gate asks.

**A 14-run gate on this class will not always come back clean**, and that is
worth knowing before someone reads a red run as a regression. Earlier rounds on
the same branch lost one run to a stall (`main` parked in `Thread.join()` inside
Spring Boot's two-thread `OnClassCondition` filtering) and one to a SIGSEGV in
an unmapped code buffer — about 2 events in 54 runs. Both are filed:
`docs/internal/fixed-suite-bugs/springboot/onclasscondition-join-never-returns-20260801-FIXED.md`
and — root-caused and closed on 2026-08-03 —
`docs/internal/jit-code-buffer-released-outside-retirement-queue-fixed-20260803.md`,
each with its sample size stated. Arm `--stack-dump-on-timeout` when running the
gate; the first stall was killed by the harness with no dump and cost the
information.

### Item 3 (`DeferredLogFactory.getLog`) now has a negative probe

This document records the receiver mix-up as never root-caused. It still is —
but it is now also unreproduced against a driver built for its exact shape.
`probes/SelfOverloadReceiverProbe.java` drives an interface `default` method
forwarding to a same-named abstract overload of itself, with a lambda capturing
the `Class` argument, at a polymorphic call site, for 400 000 iterations. Clean
on HotSpot 25 and on CratonVM at the default threshold and at
`CRATONVM_JIT_THRESHOLD=5`. Zero `NoSuchMethodError` of any kind also appeared
across five full runs of this class on four different binaries, including the
unfixed one.

That is not a root cause and does not retire the item — the advice above stands:
if it returns, treat it as a separate defect. It does mean the next person
starts with a driver instead of a signature.

### Localisation, for the next report of this shape

The defect was narrowed by JIT admission alone, on one binary (dev
`b56da0bba1`), before any source was read:

| arm | result |
|---|---|
| HotSpot 25 | PASS 26/26 |
| CratonVM, JIT on, default flags | **FAIL 26 tests / 23 failed** |
| `CRATONVM_JIT_DENY=org/springframework/boot/context/properties/bind/` | PASS 26/26 |
| `CRATONVM_JIT_DENY=java/util/` | FAIL 23 — not the JDK collections |
| `CRATONVM_JIT_DENY=BindConverter.convert` | PASS 26/26 |
| `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1` | PASS 26/26 |

`CRATONVM_JIT_DENY` bisecting a package down to a single method, then a
behaviour flag naming the mechanism, took this from "23 tests fail" to
"`run_jit_callee_handler` for a method the precise-frame relaxation admits"
without a disassembler.
