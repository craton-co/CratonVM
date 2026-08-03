# A published JIT code buffer could be unmapped outside the retirement queue

**Status: FIXED 2026-08-03** by `fix/jit-code-buffer-uaf-20260803`.

Supersedes `docs/known-issues/jit/sigsegv-in-unmapped-code-buffer-20260801.md`,
which was filed on one SIGSEGV in 54 runs of
`BasicErrorControllerIntegrationTests` and closed as "cause not located, one
occurrence". The cause was located by **not** chasing the crash. The invariant
that crash would have violated is checkable on every run, and on `origin/dev`
`387fc8e0f8` a single clean run of that same class violates it **90 times** —
17 of them with threads inside compiled code.

## 1. The reported symptom

```
#  SIGSEGV at pc=0x723bd854e15e, addr=0x723bd854e15e, pid=3743400
#  code_frees_total=0x84
#  fault pc is inside a RECENTLY FREED code buffer: base=0x723bd854e000 len=0x2580 active_jit_executions_at_free=0x2
#  fault pc is in NO live registered code buffer
#  maps: fault pc is NOT MAPPED - it is the hole between these two
```

`pc == addr` is an instruction-fetch fault: a thread was executing at that
address and the page went away. Same family as
[`jit-cache-retirement-unmaps-executing-code-fixed-20260728.md`](jit-cache-retirement-unmaps-executing-code-fixed-20260728.md).

## 2. Why the report could not be read

The predecessor doc suspected `active_jit_executions_at_free` of lying and gave
one reason — an experimental code-buffer retry on the branch it was seen on. The
truth is worse and has nothing to do with any branch. The number is unreadable
on its own for **two structural reasons**:

1. **It is recorded for buffers nothing could point into.** Every backend
   allocates its `ExecutableBuffer` *before* emitting, so every bail — a
   resolver miss, a size-estimate overrun, a raced OSR-trampoline emission —
   drops a buffer while other threads run compiled code. 8 of the ~865 frees in
   each run below are exactly this, on `dev`, with no retry in sight.
2. **It is sampled at the `munmap`, not at the decision.** `defer_jit_owner`
   proves quiescence and *then* drops the `Arc`; between the proof and
   `ExecutableBuffer::drop` any number of threads can enter compiled code. A
   perfectly legal reclamation therefore routinely records a non-zero count.

So `active_jit_executions_at_free=2` was equally consistent with a correct free,
a harmless free, and the real bug — which is why 34 runs could not separate
them.

Both facts are now recorded (`CODE_FREE_PUBLISHED`, `CODE_FREE_AUTHORISED`) and
the crash handler prints the interpretation instead of leaving it to the reader:

```
#    NEVER PUBLISHED: a discarded compile attempt. No cache entry, baked call
#      or trampoline could name it, so the count above is not evidence of anything.
#    published, released BY THE RETIREMENT QUEUE with a quiescence proof. A
#      non-zero count above only means some OTHER thread entered compiled code
#      between the proof and the unmap.
#    *** published body released OUTSIDE the retirement queue. This IS a
#      use-after-free of executable memory. Re-run with
#      CRATONVM_DBG_JIT_CODE_FREE=1 to name the release site. ***
```

## 3. What was actually wrong

The rule the codebase intends is: **a published compiled body's mapping may be
returned only from a reclamation the retirement queue proved safe.** Nothing
enforced it, and three channels broke it.

### 3.1 The entry path that held no reference at all

`try_call_compiled_entry_reentrant` (`vm/src/jit/helpers.rs`) — the helper that
calls a raw compiled entry from inside compiled code — resolved its callee
through `lookup_jit_code_range`, which returns a bare `usize`, and dereferenced
it. Its `SAFETY` comment read:

> the JIT code-range registry owns this CompiledMethod while its entry remains
> callable

The registry stores an address. It owns nothing. `pin_jit_code_range_owner` had
already been added for precisely this class of mistake — and says so in its own
doc comment — but this call site was never migrated.

This is the load-bearing one: it made JIT→JIT dispatch **the one way into a
compiled body that holds no owning reference to it**, so a reference count
reaching zero proved nothing about whether a thread was inside.

### 3.2 The per-thread dispatch caches released their own keep-alive

`DISPATCH_CACHE` / `VIRTUAL_DISPATCH_CACHE` store
`(raw entry, needs_context, Arc<CompiledMethod>)`. The `Arc` is the *only*
keep-alive for the raw pointer: `try_mic_rust_cached_entry` reads
`entry`/`needs_context` out of the map and calls the address without cloning the
owner.

Those maps are cleared by `flush_raw_entry_dispatch_caches` and
`flush_class_identity_dispatch_memos`, which run **from the dispatch helper
itself**, i.e. from inside compiled code. So the eviction runs while the
evicting thread may be executing the very body whose last owner it is dropping,
and `HashMap::clear` unmaps the code under that thread's own return address.
Every sampled instance of this channel fired with `active_jit_executions >= 1`.

### 3.3 The per-thread invoke cache had the same shape

`JvmThread::invoke_cache` (`InvokeCache<Arc<CompiledMethod>>`) holds the JIT arm
of `CachedInvokeTarget`. It is evicted by the thread dispatching through it:
`get` auto-evicts a stale entry, `put` replaces one, `evict`/`clear` drop whole
call sites. Once the JIT cache has retired a body, this entry is its last owner
— and its transient per-call clones are last owners too.

## 4. The fix

* **The dispatch helper pins.** `try_call_compiled_entry_reentrant` now holds an
  owning `Arc<CompiledMethod>` across the whole call. With that, the
  process-wide invariant is true and stated where it can be checked:

  > A thread inside a compiled body always holds an owning reference to it — so
  > a reference count reaching zero is itself a proof that no thread is inside.

  To keep that affordable per dispatch, `JitCodeRange` now carries a
  `Weak<CompiledMethod>` and `pin_jit_code_range_owner` upgrades it lock-free.
  It previously took the `jit_entry_owners` mutex, which is *global*; putting
  that on a per-dispatch path would have reintroduced exactly the cross-thread
  serialisation `cratonvm_types::striped_counter` exists to remove.

* **`cratonvm_jit::RetainedCode`** — a keep-alive whose `Drop` routes through
  `defer_jit_owner`. `DispatchCache::_owner` and the invoke cache's `JitMethod`
  parameter use it, so `clear`, `remove`, map replacement and thread-exit
  teardown all release through the queue without any of those call sites
  knowing they had an obligation. A drop that is provably not the last
  reference costs one relaxed load — that is the per-call clone path, and
  `probes/CallFloorProbe` shows no change on it.

* **The invariant is counted, not commented.** `published_code_free_audit()`
  returns `(published_frees, releases_that_skipped_the_queue)`; the second must
  be zero. `CRATONVM_DBG_JIT_CODE_FREE=1` backtraces each violation — that is
  what named all three channels above in one run, and it is what any future
  channel will be caught by.

## 5. Measurement

`BasicErrorControllerIntegrationTests`, same host, same fixture, same binary
lineage (the "before" arm is this branch with only the behavioural fixes
reverted, so the counters are identical in both):

| arm | code-buffer frees | never published | queue-authorised | **published, UNQUEUED** |
|---|---|---|---|---|
| before | 857 | 8 | 759 | **90** |
| after | 867 | 8 | 859 | **0** |

`active_jit_executions` at the 90 unqueued releases: 73 at 0, and **17 with
threads inside compiled code** — 10 at 1, two at 2, three at 3, one at 4, one at
7.

Channel attribution over the first 32 (the backtrace print cap):

| channel | count | with threads in JIT |
|---|---|---|
| `InvokeCache` / `CachedInvokeTarget` | 13 | 4 |
| interpreter transient `Arc` locals | 16 | 0 |
| `DispatchCache` (`DISPATCH_CACHE` / `VIRTUAL_DISPATCH_CACHE`) | 3 | 3 |

Tests: 26/26 on both arms. `cargo test -p cratonvm-jit --lib` 1810/1810,
`cargo test -p cratonvm-vm --lib` 2372/2372.

Throughput, `probes/CallFloorProbe 20000000 2000`, three interleaved pairs
(INVOKE ns/op):

| rung | before | after |
|---|---|---|
| invokestatic leaf | 9.09 / 9.03 / 9.05 | 9.41 / 9.12 / 9.05 |
| invokevirtual leaf | 26.50 / 26.37 / 25.33 | 26.02 / 25.57 / 25.84 |
| invokeinterface leaf | 26.46 / 25.96 / 25.39 | 22.94 / 25.47 / 25.85 |
| `String.length()` | 21.96 / 20.98 / 21.45 | 19.22 / 21.67 / 21.66 |

## 6. What is NOT claimed

The 2026-08-01 SIGSEGV was **not** reproduced, before or after. At one event in
54 runs, reproducing it as an acceptance gate was never affordable — which is
the whole reason this closes on the invariant rather than on the crash. What is
established is narrower than "the crash is gone", and stronger than a passing
rerun would have been:

* the family's defining invariant was broken on `dev`, in the class the crash
  was reported from, 90 times per clean run, with exactly the shape the crash
  report describes — a published body unmapped with no proof that no thread was
  inside it;
* 17 of those releases happened with 1–7 threads demonstrably inside compiled
  code;
* the count is now zero, and a regression test
  (`the_free_audit_records_a_release_that_skipped_the_queue`) drives the counter
  off zero on purpose, so "zero" is not vacuous.

## 7. Lessons that outlive this fix

* **An address-keyed registry that hands out a bare pointer owns nothing.** The
  `SAFETY` comment on `try_call_compiled_entry_reentrant` asserted ownership the
  data structure could not provide, and it survived several rounds of work on
  this exact bug family because it *reads* like a proof. The sibling function
  that fixes it was already in the file.
* **A per-thread cache is evicted by the thread using it.** Any "the owner
  outlives the raw pointer" argument has to survive the case where the owner is
  dropped by the very frame the pointer is executing in.
* **A diagnostic counter must say what it counts.** `active_jit_executions_at_free`
  was the sharpest tool the codebase had for this family and it was blunt in two
  independent ways, both invisible at the call site.
* **Prefer checking the invariant to reproducing the crash.** A 2%-per-run crash
  buys ~34 runs of nothing per data point. The same defect appeared 90 times in
  the first instrumented run.

## 8. Affected classes

- `module/spring-boot-webmvc` —
  `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`,
  where it was reported and measured. Nothing is specific to it: it boots a
  Spring context per test, which is a lot of first-time compilation and
  therefore a lot of retirement.
