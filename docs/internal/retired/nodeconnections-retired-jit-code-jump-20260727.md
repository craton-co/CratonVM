# NodeConnectionsServiceTests SIGSEGV — FIXED: the callee-compile probe handed out an entry address whose artifact it had already released

**Status: CLOSED 2026-07-27.** 0 SIGSEGV in 40 runs at
`CRATONVM_JIT_THRESHOLD=1`; the same-session pristine-`dev` baseline is 4/20.
The `org/elasticsearch/` JIT ban this crash had been keeping alive is removed in
the same change.

## The bug

`try_jit_compile_callee` (`vm/src/runtime/interpreter.rs`) returned
`(entry_address, needs_context)` — a **bare code address** — and dropped its
`Arc<CompiledMethod>` on the way out. Every caller then kept using that address
well past the return:

* `jit_invoke_virtual_mic` publishes it into a `JitMICSlot` / `JitPICSlot`,
  which generated code calls as `MOV R11,[slot+disp]; CALL R11`;
* `jit_invoke_dispatch` and the `java/lang/Double.valueOf` fast path call it
  through `try_call_compiled_entry_reentrant`;
* the eager invokestatic path bakes it into the machine code it is emitting.

`JitCache::put` **replaces** the body stored under a key. The superseded
artifact's last `Arc` therefore drops, `ExecutableBuffer::drop` runs, and the
code is `munmap`ped. Any concurrent tier-up publication of the same method
between the probe returning and the caller using the address turns that address
into a hole in the address space — or, once the allocator hands the same range
to a later compilation, into *a different method's body*.

`java/util/concurrent/LinkedTransferQueue$DualNode.matched()Z` is the body the
crash handler named: a tiny, extremely hot method that a concurrency-heavy test
recompiles (and therefore supersedes) constantly.

Note the stale belief this contradicts, still recorded at the C1→C2 supersede
site in `background_compile_task`: *"the old artifact is retained forever —
executable code is never freed"*. It is not; `put` drops it.

### The fix

`try_jit_compile_callee` (and `try_jit_compile_callee_slow`, and
`helpers::try_compile_callee`) now return
`(Arc<CompiledMethod>, entry, needs_context)`. The slow path re-reads the
artifact out of the cache *after* its `put`, so what comes back is whichever
body actually won a concurrent publish race. Every caller binds the `Arc` for as
long as it calls, caches or bakes the address; the eager invokestatic path
collects them into `baked_callee_pins`, which lives until the caller artifact is
published (publication is what roots baked callees in
`_direct_callee_roots`).

Holding the artifact is also what makes the inline-cache publications inside
that window able to take their own keep-alive: `resolve_jit_entry_owner` can
only succeed while the artifact is alive.

### A second, independent defect fixed alongside

`jit_invoke_virtual_mic`'s "class cached, target unresolved" branch published
`mic.cached_entry_ptr` with a **raw atomic store**, bypassing
`JitMICSlot::update` — the only writer that also resolves and retains the
callee's `Arc` in the slot's `compiled_owner`. That branch is reached after
every `clear_compiled_entry` (which keeps the class id and zeroes the entry),
so it was not a corner case. Introduced by `4f280090f`, the commit that flipped
`direct_virtual_compiled_callee_entry_enabled()` default-ON. On its own it moved
the crash rate 4/20 → 2/20; it is fixed by routing through `update`.

Both slot kinds now also refuse to publish an entry they cannot retain
(`jit_entry_publishable`): an address that is either a registered
`jit_entry_owners` key with a dead `Weak`, or inside a live JIT code region, but
has no upgradable owner, is a compiled body nothing keeps alive. Addresses that
were never JIT-managed (native targets, unit-test sentinels) stay publishable.
`jit/src/lib.rs` has two regression tests for this
(`mic_update_retains_the_callee_artifact`,
`inline_caches_refuse_a_dead_artifact_entry_but_allow_unregistered_ones`).

## Evidence

| build / config | SIGSEGV per 20 runs (`CRATONVM_JIT_THRESHOLD=1`) |
|---|---|
| pristine `dev` `2f7741c6e` | **4** |
| + MIC owner fix only | 2 |
| + MIC owner fix, `CRATONVM_JIT_DISPATCH_CACHE_VIRTUAL_DIRECT_ENTRY=0` | 0 |
| + callee-artifact pin (the fix) | **0** (0/40 over two runs) |

Repro (unchanged):

```bash
ES=/data/data/es-fixture-ivfknn-slicesdense-closure-20260717
CP=$(cat /data/tmp/es-full-cp-single-line.txt)
<cratonvm> --java-home /home/victor/jdk25 -Dtests.seed=DEADBEEFDEADBEEF \
  -Dtests.asserts=false -Des.path.home=$ES -Djava.awt.headless=true \
  -cp "$CP" org.junit.runner.JUnitCore org.elasticsearch.cluster.NodeConnectionsServiceTests
```

`NodeConnectionsServiceTests.testDisconnectionHistory` fails (`rc=1`) on every
run, before and after, on both the fixed and the pristine build — a functional
assertion failure unrelated to this crash, and not a regression from it.

## The Linux crash handler gap — also fixed, and it is what closed this

`install_crash_handler()` (Unix `SIGSEGV`/`SIGBUS`/... handlers +
`hs_err_pid*.log`) was never called on Linux: `vm-cli/src/main.rs` calls only
`install_hardware_fault_handler()`, which was a no-op off Windows. That is why
the original investigation needed an external `LD_PRELOAD` shim and never got
past "it is a retired buffer".

`install_hardware_fault_handler()` now installs the Unix set too (it does not
touch the panic hook, so it still composes with `vm-cli`'s own), and the handler
was upgraded from `signal(2)` to `sigaction(2)` with `SA_SIGINFO | SA_ONSTACK`,
so it reports the faulting **pc** and **address** instead of the old `pc=0x0`
placeholder, plus `r10`/`r11` and a dump of the inline-cache slot `r10` points
at. Under `CRATONVM_DBG_JIT_NAMES=1` it also names the compiled method
containing the pc and the address. That is the line that identified this bug:

```
#  SIGSEGV at pc=0x7e1ee4634000, addr=0x7e1ee4634000, pid=3530699, ...
#  jit pc  : java/util/concurrent/LinkedTransferQueue$DualNode.matched()Z
#  jit addr: java/util/concurrent/LinkedTransferQueue$DualNode.matched()Z
```

## Hypotheses this closes, and one it refutes

The three mechanisms the original investigation refuted stay refuted, and the
reason is now clear: none of them was the holder, because the stale address was
never *held* by anything — it was handed out by the compile probe and used
directly.

**Refuted here:** "almost certainly the same bug as the json-smart parse
corruption" (`docs/internal/jit-virtual-direct-entry-json-corruption-20260727.md`,
closed 2026-07-28).
That doc proposed its own test — run the json-smart probe under
`CRATONVM_JIT_POISON_FREE=1`, which never unmaps or recycles a retired buffer,
and a stale call becomes a crash. It does not: the probe still returns wrong
results at the same rate under `POISON_FREE`, with **no** SIGSEGV, so that
corruption is not a call into retired or recycled code. It also survives this
fix (2 errors per 1.5M ops, unchanged). It is a separate defect and its doc
stays open; see it for the current narrowing.

## Diagnostics retained (all default-off)

| flag | effect |
|---|---|
| `CRATONVM_JIT_POISON_FREE=1` | retire buffers with `mprotect(PROT_NONE)`; never unmap or reuse the address |
| `CRATONVM_JIT_LEAK_CODE=1` | never release retired `CompiledMethod`s |
| `CRATONVM_DBG_JIT_PIN=1` | report `pin_jit_entry` misses and unrooted baked callees |
| `CRATONVM_JIT_STRICT_CALLEE_ROOTS=1` | refuse to publish a body whose baked callees cannot all be pinned |
| `CRATONVM_DBG_JIT_STALE_IC=1` | at each retirement, name any inline-cache slot still pointing into the body being unmapped; also report unowned entry publications |
| `CRATONVM_DBG_JIT_NAMES=1` | record `entry→method name`; consulted by the crash handler |

`docs/known-issues/repros/nodeconnections-segv-20260727/segvtrap.c` (the
`LD_PRELOAD` `SA_SIGINFO` shim) is kept, but the in-VM handler now supersedes
it. Note for anyone reading `/proc/self/maps` from a shim: use a ≥1 MiB buffer,
this VM has thousands of mappings and a 64 KiB read silently truncates.
