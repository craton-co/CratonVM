# Bug E — Mockito/ByteBuddy mock-generation hang (OPEN)

| | |
|---|---|
| **Kind** | Hang (TIMEOUT @ 600s) |
| **CratonVM** | TIMEOUT · **HotSpot** OK |
| **Status** | OPEN — the dominant genuine residual in the TIMEOUT cluster |

## Symptom

The heavy `consumer.internals.*` / `consumer.*` / `admin.KafkaAdminClientTest` tests hang.
Distinct from the now-FIXED Bug C (WeakHashMap stream JIT) and Bug D (CompletableFuture async):
re-running these on the **A+C+D-fixed** binary, they **still TIMEOUT**.

## Root cause (localized)

`--stack-dump-on-timeout` of `FetchCollectorTest` shows the main thread deep in **ByteBuddy /
ASM bytecode generation** — `net/bytebuddy/dynamic/scaffold/TypeWriter$Default.make` →
`AsmClassReader` → `ClassReader.readCode` → `Advice` instrumentation — i.e., **Mockito inline
mock-class generation**, not the async/stream paths. The hang is in driving ByteBuddy's
bytecode transform under CratonVM (related to the Bug B / `ByteBuddyState.make` family — see
`redefine-structural-check-order-sensitive` memory and the JIT virtual-dispatch-bail note).

## Scope

A large fraction of the 68 residual TIMEOUTs are Mockito-heavy consumer/admin tests. Exact
count is contention-inflated (the sweep + re-verify overlapped concurrent builds); needs an
idle re-run to separate genuine ByteBuddy hangs from contention false-timeouts.

## Relation to other work

- Bug B (`fix/bug-B-mockstatic-dispatch`) already addresses parts of the Mockito-inline path
  (redefine + native shadowing) but is not fully on dev and the capturing-lambda mockStatic +
  ByteBuddy build-chain issues remain.
- The JIT `ByteBuddyState.make` ban (skip-list) and the virtual-dispatch-bail fix are adjacent.

## Next steps

Idle re-run of the Mockito-heavy TIMEOUT classes with `--stack-dump-on-timeout` to confirm the
ByteBuddy frame, then bisect the ByteBuddy transform path (TypeWriter/Advice) — likely a JIT
or invoke-dispatch defect in the ASM/Advice hot loop.
