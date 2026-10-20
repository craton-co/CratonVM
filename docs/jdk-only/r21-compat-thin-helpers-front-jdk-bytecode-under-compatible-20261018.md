# Under `--compatible`, thin JIT direct helpers stand in front of JDK bytecode the interpreter runs

> **Moved (JIT round 21 orchestrator, 2026-10-19)** from `docs/known-issues/jit/`: the fix is a native classification decision (a `NativeKind` / `HotSpotAlignment` row), which belongs to a JDK-only round, not a JIT change. No Java-visible answer is known to differ on a real JDK boot.


Status: OPEN (filed JIT round 21 wave 4, lane compat; classification belongs to a JDK-only round)
Area: JIT <-> native-registry dispatch agreement (`vm/src/jit/helpers.rs` thin direct helpers, `jit/src/lib.rs` direct-bind ladders)
Found by: JIT round 21 wave 4, lane compat (review of the door agreement with `resolve_dispatch`)

## What is wrong

AGENTS.md: real class bytes beat a registered `Bridge` or `SyntheticStub`; only a reviewed `Intrinsic` may win. Since
`0dac905fcf` (2026-10-08) every interpreter door and the generic tail (`vm_exec::invoke_or_native`, through
`registered_native_yields_to_loaded_bytecode`) honour that in BOTH modes.

The thin direct helpers do not under `--compatible`:

* bind time: `jit/src/lib.rs` `direct_native_helper` asks the VM only when `jdk_only` is set; the OSR door's
  `admit_direct_native_entry` -> `direct_native_bind_admitted` returns `true` for every unclassified triple in
  `--compatible` (except the two collection rows, `compat_row_refuses_collection_bind`);
* call time: `jit_direct_helper_refused` returns `false` for every unclassified gate in `--compatible` (except
  `chm_gate_under_jdk_model` / `hashmap_gate_under_jdk_model`).

So compiled code runs the VM-side body for these `--compatible` rows (kinds from
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`, all with concrete JDK 25 bytecode), while a cold or interpreted
call runs the JDK body:

| helper | registered row (kind) |
|---|---|
| `jit_preconditions_check_index_direct` | `jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I` (synthetic-stub) |
| `jit_reachability_fence_direct` | `java/lang/ref/Reference.reachabilityFence(Ljava/lang/Object;)V` (synthetic-stub) |
| `jit_buffer_session_direct` (fallback arm) | `java/nio/Buffer.session()` and the ten buffer classes (synthetic-stub) |
| `jit_scoped_memory_*_direct` (the stub rows) | `jdk/internal/misc/ScopedMemoryAccess.get/put*Unaligned` (synthetic-stub) |
| `jit_dbb_get_byte_direct` / `jit_dbb_put_byte_direct` | gate asks `java/nio/ByteBuffer.get(I)B` / `put(IB)` (abstract there), while the receiver's own row `java/nio/DirectByteBuffer.get(I)B` / `put(IB)` (synthetic-stub, concrete JDK body) is the one the tail resolves -- H7-1's interface/implementation split in reverse |
| `jit_concurrent_hashmap_get_direct` | `java/util/concurrent/ConcurrentHashMap.get` (synthetic-stub) with `jdk-concurrent-hash-map` off |

## Why nothing observable is known to differ

Each fast arm answers only the trivial case (in-range index, no segment / null session, exact receiver) and
declines everything that could throw to `jit_invoke_dispatch`, which then runs the JDK body. No probe has shown a
different answer. `tools/probes/jit/r21/R21CompatDispatchRows.java` rows `objects-checkIndex-*`,
`reference-reachabilityFence`, `bytebuffer-direct-*` and `chm-get-null` are the vectors; every row must print
`agree <row> yes` and match HotSpot in `--compatible`.

It is still a contract violation: a `SyntheticStub` wins over real bytes from compiled code only, which is the
"first call right, later calls wrong" shape waiting for a fast arm that is not answer-preserving.

## What would make it go away (pick one)

1. **Reclassify** each row above whose VM body is a reviewed, semantics-preserving fast path as `NativeKind::Intrinsic`
   (explicitly, at its registration site; ratchet `native-builtins/tests/stub_ratchet.rs` baseline moves, naming the
   rows), or retire the row under `--compatible` with a `HotSpotAlignment` row + `COMPAT_FAMILIES` entry. Then the
   interpreter runs the same body (Intrinsic, step 2) or the helper has nothing to stand in for.
2. Or apply COMPAT21-1's resolver question in `--compatible` too: make `jit_direct_helper_refused` and
   `direct_native_bind_admitted` ask `thin_helper_tail_yields_to_bytecode` for unclassified gates in both modes. That
   is a large perf regression (`Objects.checkIndex` is under every NIO accessor) for no known answer change, so it
   needs the A/B on `probes/NioAccessorRate.java` first and is the worse option.

For the `ByteBuffer` pair, the gate must also ask the RECEIVER class's row (as `direct_native_helper_for_impl` does
for `ConcurrentMap.get`), not the abstract `ByteBuffer` declaration.

## Reproducer

`cratonvm --compatible -Xmx512m tools/probes/jit/r21/R21CompatDispatchRows.java` against
`java -Xmx512m tools/probes/jit/r21/R21CompatDispatchRows.java`, plus `CRATONVM_DBG_DIRECT_BINDS=1` to see the rows
bound and served (`sites_bound`, `served` > 0 under `--compatible`).
