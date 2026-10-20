# Runtime (JIT memos, JNI, threads) review: residuals of gcj j3/rt3

> **STATUS (2026-10-19, gcj j3/rt3): OPEN -- found by reading on the `gcj3-rt3` branch (no build, no run); what this lane fixed is in `docs/internal/gc-round-gcj-20261019/j3-rt3-report.md` and not repeated here.** Severity by mechanism. None is on a collector's default path.

1. **(LOW, perf) The bind-off compiled `MethodHandle` path reads a flag snapshot per call.**
   `vm/src/jit/helpers.rs` `mh_constant_bind_redirect` (~32198) calls `mh_compiled_link_unbound_enabled()`
   (~32053, `runtime_flag_default_on`, two hash probes of the flag snapshot) on EVERY compiled
   `invokeExact` / `invoke` call while `CRATONVM_JIT_MH_CONSTANT_BIND` (or its `invoke` twin) is off; its
   neighbour `mh_constant_bind_enabled` is latched. `mh_compiled_site_link_enabled()` (~33549) is read only
   on a first link (fine). Closes with a per-VM latch (the `SharedVm` in hand; no new process static -- the vm
   statics ratchet). Row: `CRATONVM_DBG_FLAGREADS=1` on a bind-off `MhAdapterInvokeExactSweep` run names the key.
2. **(INFO) `ThreadRegistry::current_os_tid_claimed` (~7310) still counts unmounted claimants.** It answers
   "is this OS thread attached?" (`JNI_EDETACHED`), and every carrier that ever ran a virtual thread is itself a
   registered entry, so the answer is the same either way today; it would differ only for an unregistered OS
   thread that a continuation was mounted on, which the VM never does. Recorded so the rule of
   `thread_id_for_os_tid_inner` (this lane) is not copied there without a reason.
3. **(LOW) The leaf-window drain still waits under `inner`** (`vm/src/threading/gc_barrier.rs`
   `drain_leaf_windows` ~2421, called by the winning request at ~3675 right after `raise_stw_requested`). This
   lane made it report at 5, 10, 20 s, ... instead of once. Releasing `inner` across the wait would let an
   arrival, a blocked-region leave or a second request run against a pause whose request has not returned
   (its excused set, coverage slot and expected count are published under the same hold); a design would
   need the request to publish a "draining" state those paths respect. A debugger-suspended thread inside a
   leaf JNIEnv function is the only way to reach it.
4. **(LOW) The handshake requester's 1 s backstop is the only bound on a taken op.** `HandshakeOp` now says an
   op must not wait for a pause (`vm/src/threading/jvm_thread.rs`); nothing enforces it. A debug assertion
   (a per-thread "running a handshake op" bit asserted false in `safepoint_check`'s arrival) would make a
   violating op fail its unit tests instead of holding a pause for 1 s.
5. **(INFO) The Vert.x / XNIO death queues still exist on G1, ZGC and `CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK=0`.**
   Per VM since this lane; deleting them is proposal `gcj-j2cm2-proposal-native-threads-own-their-death`
   step 2, after step 1 (retire on every backend) is accepted by the G1 / ZGC owners.
6. **(MEDIUM, design) A foreign attachment whose locals may be held raw still never collects on a refused JNI
   allocation** (`vm/src/native/jni.rs` `foreign_attached_alloc_may_collect`). Proposal
   `gcj-j3rt3-proposal-the-foreign-ladder-pins-raw-locals-in-place-20261019.md`.
