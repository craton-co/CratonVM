# `AtomicReference.getAndSet` reached its native with no receiver during `doHeadHttp2` — the panic, the harness label, and the null it was actually reporting

## Status

✅ **FIXED 2026-09-06**, branch `fix/tomcat-statelock-and-atomicref-20260905`.
The page's crash is gone by construction, its harness complaint is fixed on the
platform that produced it, and its own root-cause hypothesis is refuted with
the correct explanation in its place.

⚠️ **One residual is deliberately NOT closed here** and moves to
`dohead-family-consolidated-history.md`, where it belongs: the *underlying*
null-reference defect this page was one face of. It is now named far more
precisely than it was (below), but it is not fixed.

**Severity as filed:** MEDIUM — a silently dropped `getAndSet` on a Tomcat NIO
worker, sometimes aborting the header write and timing the client out.

## What was wrong, and what changed

### 1. The panic — fixed structurally, not statistically

`native_atomic_ref_get_and_set` opened with

```rust
let this = unsafe_obj(args, 0).unwrap();   // util_concurrent_ext.rs:583
```

and 14 sibling natives did the same. A receiver that arrived as
`Value::Object(None)` — or an `args` slice with no receiver slot at all —
panicked inside the native. All 15 sites now go through `atomic_receiver`,
which raises the `NullPointerException` JVMS `invokevirtual` specifies and, on
the way, prints `args.len()`, every argument, the Java stack and (under
`RUST_BACKTRACE`) the Rust one.

This is not a probabilistic fix: the `unwrap` that produced the panic no longer
exists, so the reported crash cannot recur. What *can* still happen is the
underlying condition — a lost receiver — and that is now **reported** rather
than fatal. Across 65 Generational class-runs on two platforms (32 Windows,
33 Azure Linux) the report fired **zero** times.

### 2. The harness label — fixed on Azure

The page's own complaint: the classifier grepped every log for `panicked at`
and filed the class `CRASH` whether or not the process survived, so a
correctness bug sat next to real `rc=139` segfaults.
`/data/cratonvm/apps/tomcat-suite-runner/run-tomcat-suite.sh` now carries
`panic` as its own `results.csv` column, and a JUnit summary decides `status`;
`CRASH` is reserved for a run that produced no summary at all. Deployed and
verified on the host.

### 3. The page's hypothesis — refuted

The page offers "a marshalling defect at the JIT-compiled call site" as the
leading candidate. It cannot be: **every** dispatch door raises
`NullPointerException` for a null receiver *before* any native runs — the
interpreter's `invokevirtual` (`invoke.rs`), `jit_invoke_dispatch`'s
`Value::Object(None)` arm, `jit_invoke_virtual_mic`'s `receiver_raw == 0` arm,
and `try_jit_site_cached_native_dispatch`'s `raw == 0` bail. A receiver that is
null *at the native* was already null in the field it was loaded from.

The page also writes its four other per-class failures off as
"pre-existing/unrelated flakiness". They are the same defect.

### 4. "Generational-only?" — answered, with a control

The page states plainly that it does not claim GC-specificity beyond "0 seen so
far in the other two arms of this one run", and it was right to hedge: those
arms ran *different classes at different times*, so the zero was never a
control. Re-measured properly — same 8 classes, same 4-way parallelism,
collectors alternated round by round, one binary:

| collector | NPE lines | non-OK class-runs |
|---|---:|---:|
| Generational | 6 | 1/16 |
| ZGC | 0 | 0/16 |
| G1 | 0 | 0/16 |

Generational-only, confirmed. And **G1 relocates young objects too and shows
nothing**, so the defect is not "moving young" as a concept — it is the
Generational backend's young relocation specifically.

## The null it was actually reporting

The page's failure chain is one face of the DoHead family's live-reference
loss. That family is sharply reduced by the 2026-09-06 dev merge, measured with
the pre-merge binary interleaved as a control (same box, same minutes, same
parallelism, 900 s cap on both arms so a hang cannot pass as a failure):

| class set | pre-merge non-OK | post-merge non-OK | pre NPE | post NPE |
|---|---:|---:|---:|---:|
| `InvalidWrite0*` | 15/16 | 2/16 | 134 | 7 |
| `InvalidWrite1023/1024*` (this page's own rows) | 11/16 | 3/16 | 106 | 3 |
| **combined** | **26/32** | **5/32** | **240** | **10** |

The control matters more than the improvement: this family scores 6 NPEs at
4-way parallelism and **0 alone on a quiet host**, so a post-merge zero read on
its own would have been vacuous.

### The residual, named

The ~16% that survives always presents as a *messageless*
`java.lang.NullPointerException` at `Http2TestBase$TestInput.fill:1094` —
`int read = is.read(data, off, len);` — which reads as "the `private final`
`InputStream` was zeroed". **It is not.** Three instruments, each correcting
the last:

1. `CRATONVM_DBG_NPE_NONE=1` — 5 bare NPEs, 5 Rust-side raises, one-to-one,
   every one with `TestInput.fill` as the **deepest** Java frame at pc=24.
   `javap` puts pc=21 at the `invokevirtual read` and pc=24 at the `istore`
   after it, so `read` pushed no Java frame.
2. The symbolized backtrace puts the throw in the `sig.npe` **drain**
   (`jit_bridge.rs:11828`), which runs *after* the compiled callee returned
   through its epilogue — so the receiver was never null at dispatch; the
   callee raised it internally.
3. `CRATONVM_DBG_STTRACE=1` recovers the compiled frames that had already left
   the stack (`recovered=7`), and in **stack order** the deepest is
   `java/util/concurrent/locks/ReentrantLock.unlock:()V`.

`ReentrantLock.unlock()` is `sync.release(1)`. So the null is **`sync`**, a
`private final Sync` set in the constructor — and `lock()` dereferences the
same field and *succeeded*. The slot is live entering the critical section and
zero leaving it: **zeroed while the owning thread is blocked inside**. Seen on
two unrelated instances and paths — `NioSocketImpl.readLock` on the HTTP/2
client read (`read` bci 151) and `LinkedBlockingQueue.takeLock` under Tomcat's
`TaskQueue.take` on a worker thread (`take` bci 71).

It is the same relocation-gated defect as the parent family, at a lower rate:
`CRATONVM_NO_MOVING_YOUNG=1` takes it to **0/24** against **5/24** default,
three rounds, all the same direction.

**Mechanism still open** — named victim, timing window and gate, but not a
cause. Refuted already, and not worth re-running blind: the inline `getfield`
read guard (`CRATONVM_JIT_GETFIELD_HELPER=1`, 118 vs 121) and every compiled
store path together (107 vs 125).

## Why the symptom named the wrong object for three sessions

Every JIT-originated NPE in this VM is messageless regardless of
`-XX:+ShowCodeDetailsInExceptionMessages` / `CRATONVM_HELPFUL_NPE_OPCODES`: the
compiled null-check stub records a JEP 358 action code and the drain that
materialises the exception **discards it**. `jit_npe_message_gated` has no
production caller (only its own unit test), `take_jit_pending_npe_action()` is
called as `let _ = …`, and all four pending-NPE drains in `jit_bridge.rs`
construct `message: None` unconditionally. The interpreter, by contrast, builds
the HotSpot text correctly — verified against real HotSpot on a fixture, both
printing `Cannot invoke "java.io.InputStream.read(byte[], int, int)" because
"this.is" is null`.

So the bare NPE looked like a signal and was noise, and JUnit never saw the six
JDK frames below `TestInput.fill` that held the answer.

## Regression vectors

* `native-builtins/src/util_concurrent_ext.rs` — `atomic_receiver` +
  `report_lost_atomic_receiver` cover all 15 `Atomic{Long,Reference}` natives.
* `CRATONVM_DBG_REFPROC_AUDIT` (added this session) audits every heap write the
  reference subsystem makes and every restore it declines — it is what
  exonerated that subsystem here (59,653 pre-GC referent nulls, **every** target
  a genuine `java.lang.ref.*`, zero declines on shape/stamp/fields).
