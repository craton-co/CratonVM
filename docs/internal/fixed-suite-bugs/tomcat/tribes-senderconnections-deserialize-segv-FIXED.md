# TestGroupChannelSenderConnections — intermittent SIGSEGV / deserialize corruption

**Status:** ✅ **FIXED** (2026-07-28). Two independent use-after-free bugs, both
in the same test. Retired from `known-issues`.

Neither is Tribes-specific — this class is just a cheap reproducer, because
`testConnectionLinger` sends its 3 messages back-to-back with **zero delay**
(the other two space them 1–2 s apart), so the pooled sender is reused hard and
the receiver deserializes under allocation pressure.

## Symptom

Roughly 1 run in 6: a SIGSEGV, or one of two non-fatal siblings — an assertion
failure, or `GroupChannel.messageReceived Unable to deserialize
message:[ClusterData…]`. All three turned out to be the *same* reclaimed
object, differing only in what the program did with it next.

## Cause 1 — the pending finalize() queue was not a GC root ✅

Fixed in `aa4eca08d`, see
[22-tribes-realnetwork-membership-bug-FIXED](22-tribes-realnetwork-membership-bug-FIXED.md)
for context. `mark_finalizer_enqueued` drops an entry from
`finalizer_referent_addresses`, so once queued the only reference to the object
was the finalization queue's raw address; the non-moving young sweep freed it
and `run_finalizers` faulted in `heap.class_id_of`. Separately, only the
`System.gc` path passed finalizable roots at all — the four allocation-driven
`collect_garbage` sites passed `&[]`.

Effect, 24 runs each: baseline **3 of 4 crashes at `run_finalizers`**; after,
**0 of 6**.

## Cause 2 — HashMap.readObject held its receiver in a bare Rust local ✅

`native_hashmap_read_object` captured the map receiver `this` and the
`ObjectInputStream` `ois` into bare Rust locals, then looped
`ctx.invoke(ois, "readObject", …)`. Each call replays an arbitrary nested
object graph, so it allocates and can collect — and the "non-moving" young
sweep still **selectively promotes** survivors into old gen. Neither local is
visible to the collector's root scan.

One bug, two faulting sites, which is why it read as two:

| stale local | fault |
|---|---|
| `ois` — passed as the receiver of the NEXT `readObject` | interpreter getfield receiver barrier, `VmHeap::load_and_forward` |
| `this` | `native_map_put_evict`'s `object_num_fields` |

Fixed by pinning both across the replay (`pin_native_root` before the first
GC-capable call — `defaultReadObject` already replays the map's own fields —
and `read_native_pin` after every one), plus pinning `key` across the second
`readObject`. That is the idiom already used ~1100× in the same file. Pins only
keep objects alive and re-read forwarded addresses, so the change cannot alter
semantics.

## Verification

`TestGroupChannelSenderConnections`, 4-way parallel (the class starts its
channels with `SND_RX_SEQ|SND_TX_SEQ` only — no membership service, so no
multicast group and the receiver ports auto-bind, which makes it safe to run
copies concurrently):

| build | result |
|---|---|
| baseline | 3 CRASH + 2 other non-PASS / 24 |
| + cause-2 fix | **24/24**, then **32/32**, then **32/32** clean |
| detector armed, + fix | **0 hits / 24** |
| `--nojit`, baseline | 16/16 clean (the JIT only shifts GC timing) |

Full tribes sweep after the fix: **16 PASS / 1 FAIL**, matching the HotSpot
control. The FAIL is `TestEncryptInterceptorLargeHeap`, which fails on **both**
VMs at the ad-hoc `-Xmx2g` used by the sweep; through the suite runner (which
bumps it to 12 g) it passes.

## How it was actually found — and two wrong turns worth not repeating

The crash report is actively misleading and cost two builds:

* Its Java frame list is explicitly *"published at the last blocking/safepoint
  deposit"* — it names where the mutator last parked, **not** where it faulted.
  It pointed at `NioSender.keepalive`, which had nothing to do with either bug.
* Its three `external/jit` native frames sit at **byte-identical addresses in
  every crash**, including ones with completely different faulting PCs. They are
  the Windows exception-dispatch path, not compiled Java. "The fault is reached
  from JIT-compiled code" was wrong.

Symbolize with `CRATONVM_SYMBOLIZE=<rvas>` run **from the build tree** — a
copied-out `.exe` resolves everything to `<unresolved>` because the 97 MB PDB
must sit beside it.

**Do not bisect this with `CRATONVM_JIT_DENY` or the xt-takeover gates.** At a
~12–25 % failure rate over 24 runs those A/Bs measure timing perturbation, not
cause. Concretely: `CRATONVM_JIT_DENY=jdk/` read as a clean fix (0/16), but
denying the single `jdk/` method that actually compiles gave **6/24** — worse
than baseline. Disabling the xt JIT takeover gave 1/24 vs 3/24, inside noise,
and it is a root-*adding* mechanism so it could not have helped anyway.

What worked was a **deterministic detector**: `CRATONVM_DBG_DEADRECV`
(temporary, not retained) validated the getfield/putfield receiver against
`is_addr_live` *before* `load_and_forward` dereferenced it, and dumped the
victim's Java frames. Over 16 runs it fired on exactly the 3 non-PASS runs and
none of the 13 passing ones — zero false positives, zero false negatives. That
named the victim (`ObjectInputStream.readObject(Class)` on a
`Tribes-Task-Receiver`), proved the deserialize failures and the SIGSEGV were
the same bug, and — via the one crash it did *not* catch — pointed at the native
HashMap path.

Two structural theories were killed by that detector after looking right in the
source, which is the reason it was worth building:

* *The xt JIT-takeover conservative scan misses interpreter frames.* True as
  written — `xt_root_scan::scan_context` reads only the register file and
  machine stack, and `JvmThread::frames` is a Rust-heap `Vec` it cannot see —
  but disabling the takeover entirely still produced 3/16 detector hits.
* *A heap-address predicate blind spot filters live operand-stack roots out.*
  Refuted by the detector's clean record on passing runs: the same code runs
  there and never trips, so the object really was reclaimed.

## Reproduction

```powershell
$env:CRATONVM_REAL_NET_SOCKETS='1'; $env:CRATONVM_REAL_AQS='1'
<cratonvm.exe> -Xmx2g -Djava.net.preferIPv4Stack=true -cp <tomcat suite cp> org.junit.runner.JUnitCore org.apache.catalina.tribes.group.TestGroupChannelSenderConnections
```

~24 copies 4-way parallel; pre-fix, expect 4–6 access violations.
