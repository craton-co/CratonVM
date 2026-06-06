# Regression-pool coverage gap: non-gated NIO/buffer/VarHandle changes are untested

**Severity:** medium (the default-mode "no regressions" claim for several non-gated changes rests on a pool that never exercises them). Self-contained. Baseline commit: `d6cefc7` on `dev`.

## Problem
Commit `d6cefc7` shipped several changes that are **active in the DEFAULT build** (NOT behind `CRATONVM_REAL_NET_SOCKETS`):
- `dbb_allocate_direct0` DirectByteBuffer layout fix (`native-io/src/direct_buffer.rs`).
- `NativeContext::copy_from/to_native_memory` raw/arena routing (`native-api/src/registry.rs`, `vm/src/vm/vm_exec.rs`).
- `VarHandle.getAndBitwiseOr/And/Xor` (`native-builtins/src/lang_invoke.rs`).
- `Buffer$1.newDirectByteBuffer` overloads (`native-builtins/src/shared_secrets_bridge.rs`).

The regression pool (`test-infra/regression-pool/run.sh` + `pool.tsv`, 14 probes) was confirmed 13/14, BUT an adversarial review found **none of the 14 probes use** `ByteBuffer.allocateDirect`, `VarHandle`, `FileChannel`, `MappedByteBuffer`, or `Unsafe` (grep `test-infra/probes/` returned empty). So the pool gives only a shallow startup/classload signal and CANNOT catch regressions in the changed code paths.

## Goal
Add small, deterministic probes to the regression pool that exercise each non-gated change, so future changes to these subsystems are caught. Follow the existing probe convention exactly (see `test-infra/probes/*` + `pool.tsv` columns: name, app, probe_class, probe_dir, classpath_glob, args, max_seconds, baseline_file; and `baselines/<name>.expected.txt`). Probes must be hermetic (no network, no external app jars — JDK-only), print a small stable string, and be diffable.

## Probes to add
1. **DirectBufferProbe** — `ByteBuffer.allocateDirect(N)`: assert `capacity()==N`, `position()==0`; `put`/`flip`/`get` round-trip a few bytes; also a `slice`/`duplicate` sanity check. (Guards the `dbb_allocate_direct0` cap=-1 regression directly.)
2. **VarHandleBitwiseProbe** — `findVarHandle` on an instance `int` field; `getAndBitwiseOr/And/Xor` (+ at least one `getAndAdd` and `compareAndSet`) and assert old-value-returned + field-updated. (Guards the VarHandle meta path incl. the `getAndAdd` follow-up in `continue_prompt_varhandle_getandadd_meta.md`.)
3. **FileChannelProbe** — write a `HeapByteBuffer` to a temp file via `FileChannel.write`, read it back via `FileChannel.read` into a heap buffer, assert content. This forces `Util.getTemporaryDirectBuffer` (arena handle) through `FileDispatcherImpl.read0/write0` — it will FAIL/SIGSEGV until `continue_prompt_arena_native_memory_routing.md` (R2) lands, so coordinate: either add it now as a known-failing probe that the R2 work flips to PASS, or land it together with R2. (Print the round-tripped string.)
4. (Optional) **UnsafeMemProbe** — `Unsafe.allocateMemory`/`putByte`/`getByte`/`freeMemory` round-trip, asserting handle-backed memory works.

## Plan
1. Write the probe `.java` files under `test-infra/probes/<probe_dir>/` and compile them (the harness `stage.sh`/`run.sh` expects pre-built `.class` dirs — match how existing probes are staged).
2. Add rows to `test-infra/regression-pool/pool.tsv` (JDK-only classpath, small `max_seconds`).
3. Capture baselines: `bash test-infra/regression-pool/run.sh --record <name>` for each (HotSpot-correct expected output — verify each probe prints the SAME thing on stock `java` first).
4. Run `bash test-infra/regression-pool/run.sh` → confirm new probes PASS (except FileChannelProbe if R2 not yet landed — see above).

## Verification
- Each new probe prints identical output on stock `"/c/Program Files/Java/jdk-25/bin/java.exe"` and on `target/release/cratonvm.exe`.
- Full pool run is green for the new probes (and still 13/14 overall — `hadoop-conf` remains the pre-existing REGRESS).
- Deliberately reverting the `dbb_allocate_direct0` fix makes DirectBufferProbe REGRESS (proves the probe has teeth).

## Key files
`test-infra/regression-pool/run.sh`, `pool.tsv`, `stage.sh`, `baselines/`, `test-infra/probes/`. Reference probes: `test-infra/probes/wildfly_probe`, `.../classload_probe`. Memory: `reference_cross_vm_comparison_harness`, `reference_server_socket_gap`.
