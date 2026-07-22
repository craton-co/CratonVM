# Bug 21 — SIGSEGV (access violation) in the SSL/TLS network path

> **STATUS: RESOLVED on `9d8bba97` (no longer reproduces).** The crash was real on the
> pre-merge binary used for the `cv-full2` full run. After the 41-commit dev merge
> `b5c709c7` (incl. JIT null-check-elim fix `82b9bdf6` + GC fixes), a clean isolated
> rebuild (`kv2.exe`, worktree `CratonVM-kafka2`) shows **0 stale/SIGSEGV across 6+
> runs** of all three affected classes in *default* mode — they now TIME OUT on a
> separate network/socket gap (HotSpot also fails them), no crash. Root cause + the
> `CRATONVM_SHADOW_STACK` mitigation are kept below in case it recurs (it is
> GC-timing-dependent; it fired reliably within one pass on the old binary and did not
> fire in ~6×60–90 s of crash-prone SSL execution on the new one).

**Severity:** High (hard process crash — SIGSEGV, not a clean failure).
Clean-run statuses:
- `org.apache.kafka.common.network.SslTransportLayerTest` — **CRASH** (HotSpot: LOADERR — missing optional dep, so HotSpot never even runs it)
- `org.apache.kafka.common.network.Tls13SelectorTest` — **ABEND** (HotSpot: FAIL — clean test failures, no crash)

Both differ from HotSpot's behaviour (HotSpot does not segfault), so they are
reported per the task rule.

## Symptom

`EXCEPTION_ACCESS_VIOLATION (0xC0000005)` raised from inside JIT-compiled code:

```
SslTransportLayerTest:  pc=0x00007FF747D6FBEB  faulting RVA: 0x8EFBEB   Memory around R10 (0x0000000000000008): <unreadable>
Tls13SelectorTest:      pc=0x00007FF747CE0248  faulting RVA: 0x860248   Memory around R10 (0x0000000000000010): <unreadable>
```

In both the faulting address is reached *from JIT frames* (`external/jit` entries
in the backtrace) and the trap register holds a tiny value (`0x8`, `0x10`) — i.e.
a **field read at a small offset off a null/near-null base pointer**: a null
receiver flowing into JIT-compiled field access.

The native backtrace shows a tight **recursion cycle** (the same RVA set repeats
many times):

```
... exe+0x7EA9E0 -> exe+0x8E3F2C -> exe+0x8579EE -> exe+0x8450D3 -> exe+0x7F0788 -> exe+0x7DDDD5 -> (repeat) ...
fault: exe+0x8EFBEB (SslTransportLayerTest) / exe+0x860248 (Tls13SelectorTest)
```

so the crash is the tail of a deep recursive interpreter↔JIT call chain (selector
poll / SSL handshake re-entry) that eventually dereferences a null object.

## Root cause — CONFIRMED (same as bug-22)

**Conservative JIT-frame root scanning misses live references that exist only in
machine registers** (not spilled to the scannable stack). The young non-moving
sweep therefore frees a still-live object; the dangling slot is later dereferenced
in JIT-compiled code at a small field offset off the now-null/zeroed base →
`EXCEPTION_ACCESS_VIOLATION`. This is the "register-invisibility" hazard documented
in `project_precise_jit_stack_maps` / `reference_osr_main_corruptor`. The SSL/crypto
path triggers it because its heavy allocation forces a young GC at exactly the wrong
moment, while a live receiver sits only in a register.

bug-22 is the **interpreter-caught** form of the identical defect (the salvage path
degrades it to a wrong `NoSuchMethodError` instead of a hard fault).

### Evidence
| Run | Result |
|-----|--------|
| default (JIT, conservative roots) | SIGSEGV / ABEND (stale-pointer warnings precede) |
| `--nojit` | **0 stale/SIGSEGV** (fails later on an unrelated JKS-keystore gap, like HotSpot) |
| `CRATONVM_SHADOW_STACK=1` (precise JIT roots) | **0 stale/SIGSEGV** — crash eliminated; class now times out on the separate network/socket gap |

`SslTransportLayerTest` and `Tls13SelectorTest` both went from CRASH/ABEND →
stale/segv count **0** under `CRATONVM_SHADOW_STACK=1`.

## Fix

Run with **`CRATONVM_SHADOW_STACK=1`** (shadow-stack precise JIT roots) — pins
register-only references so the young sweep no longer frees them. This eliminates
the crash. It is currently default-OFF (perf/throughput tradeoff + the bt18
GC-counting caveat in `project_precise_jit_stack_maps`); promoting it to default —
at least for the kafka workload — should be evaluated against the regression pool
and bt18 before flipping.

## Reproduce

```
cd apps/kafka/tests
# crashes:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./cvk.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.network.Tls13SelectorTest
# no crash:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_SHADOW_STACK=1 ./cvk.exe \
  -cp ".;$(cat cp.txt)" KRun org.apache.kafka.common.network.Tls13SelectorTest
```

Fault RVAs for a debug-symbol symbolize: `0x8EFBEB` (SslTransportLayerTest),
`0x860248` (Tls13SelectorTest), cycle frames
`0x7EA9E0/0x8E3F2C/0x8579EE/0x8450D3/0x7F0788/0x7DDDD5` (the stock release PDB is
not consumable by the in-tree `CRATONVM_SYMBOLIZE` offline symbolizer).

## Affected classes (append more as found)
- common.network.SslTransportLayerTest (CRASH)
- common.network.Tls13SelectorTest (ABEND)
