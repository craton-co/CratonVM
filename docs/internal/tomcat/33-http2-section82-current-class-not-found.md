# 33 - TestHttp2Section_8_2 current class not found (FIXED)

**Status:** Fixed and retired from `docs/known-issues` on 2026-07-28.

## Root cause

The metadata-unloading path treated a `ClassLoaderId::UserDefined` value as a
unique Java `ClassLoader`. It is only a resolution namespace. JDK dynamic
proxies can share that synthetic namespace while their defining loaders have
independent lifetimes. When collection proved one proxy loader dead, the VM
tombstoned every class in the namespace, including live JUnit/proxy classes
with active interpreter or JIT frames. A later dispatch then failed to find
the frame's `ClassId` and reported `current class not found`.

The same stress run also exposed an independent lock-order residual: reference
queue processing held `RefProcessor` (L7) and then queried `ClassManager`
(L10) to discover `java/lang/ref/Reference.next`.

## Fix

- `ClassManager::unload_user_classes` now removes exactly the ClassIds whose
  defining loaders are known dead. `unload_user_loader` remains available for
  true whole-namespace callers. Loader bookkeeping and resolution caches are
  updated from the exact removal set, preserving live proxy siblings.
- A regression test creates two classes in the same synthetic user namespace
  and verifies that unloading one leaves the other live.
- Reference-next-slot lookup now happens before the reference-processor lock
  is acquired. Synthetic two-field references still use slot zero.
- `HashMap$KeyItr.remove()` now pins and refreshes its iterator receiver across
  the nested, GC-capable HashSet removal. This removes the stale-address
  out-of-bounds write found while auditing the no-JIT HTTP/2 matrix.
- `RunMethods` accepts `--range <first-index> <last-index>`, and
  `run-section82-shards.ps1` executes bounded ranges in either JIT mode or
  `-NoJit` mode with lock-order checking and verifies each JUnit summary.

## Validation

Built a fresh release candidate from the fixing branch and ran
`org.apache.coyote.http2.TestHttp2Section_8_2` through the real Tomcat suite
fixture with `CRATONVM_LOCK_ORDER_CHECK=1`.

| coverage | result |
|---|---|
| JIT indexes 0 through 6657 | 6,658 run, 0 failures, 34/34 bounded ranges passed |
| no-JIT indexes 0 through 6657 | 6,658 run, 0 failures, 34/34 bounded ranges passed |
| patched stale-reference range 2600 through 2799 | JIT 200/200 and no-JIT 200/200, each with zero failures |
| invariant scan | no `current class not found`, lock-order violation, panic, `InternalError`, `RESID-DIAG WRITE`, or JUnit failure in the patched focused runs |

The current Tomcat data provider exposes 6,658 cases (indexes 0-6657); the
earlier 7,682 estimate was not the fixture's actual cardinality.

## Reproduction helper

```powershell
apps\tomcat-suite-runner\run-section82-shards.ps1 `
  -Exe <cratonvm.exe> -ProbeDir <compiled RunMethods directory>

# Repeat the complete matrix without JIT
apps\tomcat-suite-runner\run-section82-shards.ps1 `
  -Exe <cratonvm.exe> -ProbeDir <compiled RunMethods directory> -NoJit
```

The helper defaults to the complete current range and rejects a shard unless
its expected count passes with zero failures.
