# NodeConnectionsServiceTests SIGSEGV — localized to a jump into RETIRED JIT code; owner path still unidentified

**Status: OPEN.** Reliable repro, decisive fault characterization, three
candidate mechanisms refuted with evidence. No fix yet — do not close.

Branch with the diagnostics: `fix/nodeconnections-segv-20260727` (NOT merged to
`dev`; it contains diagnostic-only switches that must never ship enabled).

## Repro

```bash
ES=/data/data/es-fixture-ivfknn-slicesdense-closure-20260717
CP=$(cat /data/tmp/es-full-cp-single-line.txt)
<cratonvm> --java-home /home/victor/jdk25 -Dtests.seed=DEADBEEFDEADBEEF \
  -Dtests.asserts=false -Des.path.home=$ES -Djava.awt.headless=true \
  -cp "$CP" org.junit.runner.JUnitCore org.elasticsearch.cluster.NodeConnectionsServiceTests
```

Rate on `dev` @ `2f7741c6e`: **2/20** at default JIT settings; **3/20 – 5/20**
with `CRATONVM_JIT_THRESHOLD=1`, which is the recommended amplifier for any
further work (it roughly doubles the rate and shortens the loop).

The class is concurrency-heavy (`testConcurrentConnectAndDisconnect`, transport
worker pools), which is why it surfaces this and most classes do not.

## What the fault is

Captured with a small `LD_PRELOAD` `SA_SIGINFO` reporter (`/data/tmp/segvtrap.c`,
built to `segvtrap.so`) — **gdb suppresses the race entirely**, 14/14 runs under
`gdb -batch` exited cleanly, so gdb is not usable here.

```
@@SEGVTRAP signal=11 si_code=2 si_addr=0x76e0c460a000 thread='elasticsearch[o…'
  rip=0x76e0c460a000
  map: 76e0c460a000-76e0c4610000 ---p 00000000 00:00 0
  r11=0x76e0c460a000
```

- `rip == si_addr == r11` — an indirect `call/jmp r11` landed on the target.
- The target is the **first byte of a 24 KiB anonymous region**.
- Up to **three threads fault on the same address simultaneously**.
- Without the poison switch below the region is *absent* from
  `/proc/self/maps` (`si_code=1`, SEGV_MAPERR).
- With `CRATONVM_JIT_POISON_FREE=1` — which retires executable buffers via
  `mprotect(PROT_NONE)` instead of `munmap`, keeping the schedule identical —
  the same fault becomes `si_code=2` (SEGV_ACCERR) and the region **is** present
  as `---p`.

That last A/B is the load-bearing evidence: **the faulting page is a retired JIT
code buffer**, not a wild pointer. Something holds a compiled-entry address past
the lifetime of the body it points into.

`--nojit` is **0/20**, confirming it is JIT-specific.

## Mechanisms REFUTED (do not re-litigate without new evidence)

1. **Dispatch caches holding a raw entry with no keep-alive.**
   `helpers.rs`'s `VIRTUAL_DISPATCH_CACHE` / `DISPATCH_CACHE` store
   `DispatchCache { entry, _owner: pin_jit_entry(entry) }`; if that `Weak`
   upgrade failed they would cache an unowned raw pointer. Instrumented
   (`CRATONVM_DBG_JIT_PIN=1`): **0 pin misses across 16 runs.** Every cached
   entry did hold an owner.

2. **Retired owners being dropped too early.** Added
   `CRATONVM_JIT_LEAK_CODE=1`, which makes `defer_jit_owner` /
   `drain_deferred_jit_owners_if_quiescent` never release anything.
   **leak-ON 3/16 vs leak-OFF 1/16** — leaking every deferred owner does NOT
   stop the crash. Corollary: the buffer is freed through a path that never
   reaches `defer_jit_owner` (most likely the last `Arc<CompiledMethod>` clone
   in a superseded `JitCache` shard snapshot going away after a tier-up `put`).

3. **Unrooted baked direct-call targets.** `JitCache::prepare_for_publication`
   builds `_direct_callee_roots` with
   `filter_map(|e| resolve_jit_entry_owner(e))`, which *silently discards*
   entries that fail to resolve even though the caller's emitted code still has
   them baked in. Made that countable and added
   `CRATONVM_JIT_STRICT_CALLEE_ROOTS=1` to refuse publication in that case:
   **`unrooted-callee-events=0` in 40 runs**, and strict-ON 5/20 vs
   strict-OFF 3/20. Not this either.

## Where to go next

The holder is none of the above, so enumerate the remaining raw-entry holders:

- **Inline caches embedded in generated code** (`cached_entry_ptr` in the MIC/PIC
  slots). They have a `compiled_owner` + `clear_compiled_entry`, but check for a
  path that stores `cached_entry_ptr` without installing the matching owner, or
  clears the owner while leaving the pointer non-zero.
- `cratonvm_types::jit_activation::register_executable_owner` — a second,
  independent entry registry.
- The tiered manager's own record of compiled entries.
- OSR trampolines (`emit_osr_trampoline` allocates its own `ExecutableBuffer`).

**Highest-value next step:** with `CRATONVM_JIT_POISON_FREE=1` the retired
address is stable and never recycled, so add a retired-buffer registry
(`addr, len, method name` — `register_jit_method_name` already records exactly
this under `CRATONVM_DBG_JIT_NAMES`) and dump it, then map the faulting address
to the method whose body was retired. Knowing *which* method is being jumped
into should immediately identify the holder.

**Also worth fixing on the way:** `install_crash_handler()` (which installs the
Unix SIGSEGV/SIGBUS handlers and writes `hs_err_pid*.log`) is never called on
Linux — `vm-cli/src/main.rs` only calls `install_hardware_fault_handler()`,
which is a no-op off Windows. That is why this crash produced no VM-side report
at all and an external `LD_PRELOAD` shim was needed.

## Diagnostic switches added on this branch (all default-off, diagnosis only)

| flag | effect |
|---|---|
| `CRATONVM_JIT_LEAK_CODE=1` | never release retired `CompiledMethod`s |
| `CRATONVM_JIT_POISON_FREE=1` | retire buffers with `mprotect(PROT_NONE)`, never unmap or reuse the address |
| `CRATONVM_DBG_JIT_PIN=1` | report `pin_jit_entry` misses and unrooted baked callees |
| `CRATONVM_JIT_STRICT_CALLEE_ROOTS=1` | refuse to publish a body whose baked callees cannot all be pinned |
