# `DevToolsEmbeddedDataSourceAutoConfigurationTests` — 300s TIMEOUT, zero output on either stream — root cause not confirmed

**Status: OPEN.** Not a regression of either of the two previously-FIXED bugs
this exact class was historically part of — both checked and ruled out below.

## Symptom

2026-08-06 full-suite Windows run (`craton-fullsuite-windows-20260806-s2/all-jit`),
`-Xmx 2g`, 300s/class, default Generational GC:

`org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDataSourceAutoConfigurationTests`
(`module/spring-boot-devtools`) — TIMEOUT/HANG, 300.188s.

Log: `craton-fullsuite-windows-20260806-s2/all-jit/logs/module_spring-boot-devtools.org.springframework.boot.devtools.autoconfigure-251ffbbaec3e.{out,err}.log`

`.out.log` is **0 bytes** — no Spring/JUnit output of any kind, not even the
class's own single `@Test` starting. `.err.log` is 6 lines, all from VM
startup, and stops there:

```
2026-08-07T03:00:10.929205Z  WARN cratonvm_vm::vm::vm_util: Post-clinit fixup: Unsafe ARRAY_*_BASE_OFFSET/INDEX_SCALE populated (18/18)
2026-08-07T03:00:10.953395Z  WARN cratonvm_vm::vm::vm_util: Post-clinit fixup: UnsafeConstants populated (5/5)
2026-08-07T03:00:11.289027Z  WARN cratonvm_vm::vm::vm_util: Post-clinit fixup: BigInteger ZERO/ONE/TWO/NEGATIVE_ONE/TEN populated (5/5)
2026-08-07T03:00:11.289070Z  WARN cratonvm_vm::vm::vm_util: Post-clinit fixup: BigInteger digitsPerLong/longRadix radix tables populated
2026-08-07T03:00:11.504254Z  WARN cratonvm_vm::vm::vm_util: Post-clinit fixup: File fs/separator/pathSeparator populated (5/5)
Mockito is currently self-attaching to enable the inline-mock-maker. ...
```

No further lines of any kind for the remaining ~300s until the harness kills
the process. Notably **no GC activity at all** is logged (no `[moving-young]`
fallback lines, unlike every other HANG in this same run's cluster) —
whatever the process is doing between the Mockito self-attach line and the
kill is not allocating enough to trigger a young collection, consistent with
a tight, low-allocation spin rather than a blocked-on-I/O wait that
periodically allocates.

`hotspot-baseline` is clean and fast for this class in every prior run this
suite has recorded (see below), so this is craton-specific.

## Prior history for this exact class

This class name is genuinely well-trodden ground — two *different*,
previously-FIXED bugs both listed it:

**1. `devtools-2class-host-load-confound-FIXED.md` (FIXED 2026-07-25).** A
single-thread self-deadlock: `array_is_assignable_to_impl`'s
`resolve_component` closure chained `.read()....or_else(||
load_class_concurrent(...))` in one expression, so the `RwLockReadGuard`
temporary lived through the `or_else` closure (Rust's end-of-statement
lifetime rule) and self-deadlocked against `class_manager.write()` on a genuine
class-loading miss — exactly the shape hit by Mockito/ByteByddy's freshly
generated array-component types.

*Checked against current `dev`*: the fix is present.
`vm/src/runtime/interpreter/typecheck.rs:527-543` (the function moved from
`interpreter.rs` since the fix landed) binds the read result to a `let`
before calling `load_class_concurrent`, with a comment describing the exact
historical bug — this is the fixed code, not a reverted copy.

**2. `junit5-interceptingexecutableinvoker-layout-probe-livelock-cluster-FIXED.md`
(FIXED 2026-07-18).** `DevToolsEmbeddedDataSourceAutoConfigurationTests` was
one of the *original five* classes in this cluster, and its signature is a
striking surface match for the current hang: 300s TIMEOUT, **zero JUnit
output on either stream**, "no `SBRUNNER_RESULT`". Root cause there: the
Panama foreign-function downcall adapter fast path in `try_stackless_invoke`
matched purely on method name (`invoke`/`invokeExact`/`invokeBasic`) and read
field 0 of the receiver before confirming it was actually a `MethodHandle`
subclass. JUnit5's zero-field `InterceptingExecutableInvoker.invoke` (used by
every test invocation, and reached earliest/most reliably by classes that
route through `ModifiedClassPathExtension`'s nested `Launcher.discover()` +
`execute()` — this class carries class-level `@ClassPathExclusions("HikariCP-*.jar")`,
which is exactly that mechanism) hit the same dispatch, producing a tight
retry loop and a repeating `cratonvm::gc::guard: gen_heap::get_field:
out-of-bounds field read dropped ... class_name=org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
warning at a steady ~150-400ms cadence for the process's entire life.

*Checked against current `dev`*: the fix is present.
`vm/src/runtime/interpreter/invoke.rs:3160-3183` gates the adapter fast path
on `is_method_handle_adapter` (walks the receiver's runtime class hierarchy
via `cm.is_subclass_of(adapter_class, method_handle_class)`, with the exact
comment from the fix's own writeup: *"probing field 0 before establishing
that the receiver is actually a MethodHandle subclass turns every unrelated
zero-field receiver into an OOB heap-field read"*). **And**, decisively, the
*symptom doesn't match*: the current hang's `.err.log` has none of the
repeating `gc::guard: gen_heap::get_field: out-of-bounds` warnings that were
this cluster's defining signature — not one occurrence, let alone the
steady sub-second-interval flood the old bug produced. A silenced/quieter
recurrence of the same fixed defect is not impossible, but there is no
positive evidence for it here, only the absence of the old diagnostic.

**Conclusion: neither previously-fixed bug's mechanism is confirmed present.**
Fix #1 (the `RwLockReadGuard` self-deadlock) is source-confirmed intact and
that whole function looks correct. Fix #2 (the OOB adapter livelock) is also
source-confirmed intact, and its own diagnostic signature is absent from the
current hang, which argues against it being a quiet recurrence of the exact
same code path — though it does not rule out a *sibling* bug in the same
general area (see below).

## Prior timings for this exact class (context for "this used to be fast")

| Run | Result | Seconds |
|---|---|---:|
| `craton-fullsuite-azure-20260802` | PASS | 15.307 |
| `craton-fullsuite-azure-20260805-s2` | PASS | 11.647 |
| `craton-fullsuite-windows-20260806-s2` (this doc) | **HANG** | 300.188 |

This is not a class that has ever been slow before — it went from an
~11-15s pass on Azure Linux to a full, silent 300s timeout on Windows with
literally zero forward progress recorded. That magnitude of change argues
against "the margin ran out" (contrast with
[`flyway-integration-autoconfigurationtests-300s-margin-exhausted-windows-20260807.md`](flyway-integration-autoconfigurationtests-300s-margin-exhausted-windows-20260807.md),
where both classes show continuous progress right up to the kill) and for a
genuine new stall.

## What's ruled out, and what isn't

Ruled out: the two previously-FIXED bugs above (source confirmed present,
symptom confirmed absent/mismatched for #2).

Not ruled out, not confirmed either — candidates, weakest to strongest:

- **A quiet sibling of the fixed OOB-adapter livelock.** The July fix in
  `invoke.rs` is scoped narrowly to the Panama downcall-adapter fast path
  keyed on method name `invoke`/`invokeExact`/`invokeBasic`. If some other
  speculative fast-path dispatch in the interpreter/JIT has an analogous
  "dispatch on name, read a field before confirming receiver shape" bug for a
  *different* method name, it would produce the same "zero JUnit output,
  full timeout, no crash" shape without tripping the `gc::guard` warning
  that only fires for that one specific adapter path. Nothing in this
  session identifies a candidate site — flagged as a shape, not a location.
- **A different bug in the `ModifiedClassPathExtension` nested-`Launcher`
  pathway.** This class carries class-level `@ClassPathExclusions("HikariCP-*.jar")`,
  which routes its *only* `@Test` method through
  `ModifiedClassPathExtension.interceptMethod` → a freshly-built
  `ModifiedClassPathClassLoader` (a `URLClassLoader` scanning and filtering
  the full test classpath) → a brand-new nested `Launcher.discover()` +
  `execute()` for the same test, all before any of the class's own Spring
  Boot/JUnit logging would ever run — which is consistent with zero output
  from the very first line. This pathway's own FIXED docs explicitly note it
  is fragile and has hosted at least two distinct, unrelated hangs already
  (the OOB livelock above, and a separate
  `DiscoveryIssueException`/`UniqueIdSelector could not be resolved` failure
  mode fixed 2026-07-28 in
  `modifiedclasspathextension-nested-launcher-uniqueid-discovery-failure-FIXED.md`
  — that one fails fast with an exception rather than hanging, so it is not
  a candidate for *this* symptom, but it demonstrates the pathway keeps
  producing new failure modes). The July 18 cluster doc's own text
  ("having a classpath-modification annotation neither guarantees nor is
  required for the signature") already warned against assuming
  `ModifiedClassPathExtension` usage is either necessary or sufficient to
  explain a given hang in this family — so this is a plausible mechanism,
  not a confirmed one.
- **Windows-specific.** This is (as far as this triage found) the first
  full 4-shard suite run captured against a dedicated Windows release binary
  (`cratonvm-fullsuite-20260806.exe`); every prior comparison point for this
  class is Azure Linux. `ModifiedClassPathClassLoader` construction involves
  scanning the full test classpath (potentially hundreds of jars) to build a
  filtered `URLClassLoader` — if any part of that (jar opening, path
  resolution) is platform-sensitive or synchronizes more broadly on Windows
  under the 16-way-parallel host load this run used, a slow-but-eventually-
  successful construction would be indistinguishable in these logs from
  a genuine hang, since none of that setup work produces Spring/JUnit log
  output either way. Not distinguished from a true livelock without a live
  process inspection (e.g. a stack sample), which this triage's "no
  multi-minute reruns" scope does not allow.

No debugger attach or stack sample was taken this session (out of scope per
this triage's constraints — no rebuilds, no multi-minute reruns). The next
useful step, if this recurs, is a single isolated run of just this class with
`--stack-sample-ms` (per the harness's own diagnostic support) to see what
the main thread is actually doing during the silent window.

## Affected classes

- `module/spring-boot-devtools` — `org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDataSourceAutoConfigurationTests`
