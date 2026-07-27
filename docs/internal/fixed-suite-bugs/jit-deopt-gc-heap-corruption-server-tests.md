# JIT/deopt/GC heap-corruption crashes in ES `:server` unit tests

Status: fixed

Date observed: 2026-07-04

## Fix summary

Fixed on 2026-07-08 by making two fail-closed changes in the JIT path:

- `JitCache` now retires evicted compiled methods instead of immediately unregistering and dropping executable buffers. This keeps compiled code-range metadata available after deoptimization/cache eviction unless the explicit diagnostic `CRATONVM_JIT_FREE_CODE` mode is enabled, avoiding stale compiled-call targets into freed code memory.
- The Elasticsearch interval-provider crash signature was narrowed to JIT compilation of `org/yaml/snakeyaml/emitter/Emitter.emit`. Conservative JIT eligibility now keeps that exact dispatcher interpreted in both the VM skip-list and the final `cratonvm-jit` `try_compile` gate, with `CRATONVM_JIT_ALLOW_PACKAGES=org/yaml/snakeyaml/emitter/` left as the explicit bisection escape hatch.

Validation used a remote Linux worktree at `/data/data/cratonvm-worktrees/20260708-214648-jit-deopt-gc-heap-corruption`, branch `codex/fix-jit-deopt-gc-heap-corruption-20260708-214648`, with a uniquely named release-with-debug binary:

```text
/data/data/target-jit-deopt-gc-heap-corruption-20260708-214648-rwd/release-with-debug/cratonvm-jit-deopt-gc-heap-corruption-20260708-214648
```

Focused Rust validation:

```text
CARGO_TARGET_DIR=/data/data/target-jit-deopt-gc-heap-corruption-20260708-214648-focused2 \
  cargo test -p cratonvm-vm snakeyaml_emitter_emit --lib -- --nocapture
# 2 passed

CARGO_TARGET_DIR=/data/data/target-jit-deopt-gc-heap-corruption-20260708-214648-focused2 \
  cargo test -p cratonvm-jit snakeyaml_emitter_emit_final_guard_is_exact --lib -- --nocapture
# 1 passed

CARGO_TARGET_DIR=/data/data/target-jit-deopt-gc-heap-corruption-20260708-214648-focused2 \
  cargo test -p cratonvm-jit test_jit_cache_ --lib -- --nocapture
# 7 passed
```

Elasticsearch crash regression validation used a compact five-class bundle and the same rows listed below. Before the guard, the three interval-provider classes exited as `CRASH`/rc=139 under default JIT while the same rows did not crash with `--nojit`, `CRATONVM_JIT_THRESHOLD=2000000000`, or `CRATONVM_JIT_DENY=org/yaml/snakeyaml/emitter/Emitter.emit`. After the fix, the default-JIT run completed all five rows without crashes:

```text
run: jit-deopt-gc-five-fixed-guard2-20260708-214648
mode: all-jit
results: FAIL=5, CRASH=0

org.elasticsearch.index.mapper.TsidExtractingIdFieldMapperTests             FAIL
org.elasticsearch.index.mapper.UpdateMappingTests                          FAIL
org.elasticsearch.index.query.CombineIntervalsSourceProviderTests          FAIL
org.elasticsearch.index.query.DisjunctionIntervalsSourceProviderTests      FAIL
org.elasticsearch.index.query.FilterIntervalsSourceProviderTests           FAIL
```

The remaining `FAIL` statuses are ordinary suite/runtime failures in this compact bundle, including the known native-access `NoSuchMethodError` shape, not the documented JIT/deopt/GC heap-corruption crash.

## Summary

While verifying the fix for
[`elasticsearch-mapper-query-merge-native-crashes.md`](../internal/elasticsearch-mapper-query-merge-native-crashes.md)
(moved to `..` — that bug is fixed), a full-batch rerun of the
same class range (`server` module, indices 1686-1717, JIT-on) turned up 5
CratonVM-only crashes with a **different** exception code and root cause: a
genuine `EXCEPTION_ACCESS_VIOLATION` (`0xC0000005`), not the `0xC0000409`
stack-buffer-overrun from the fixed bug. These reproduce **deterministically,
even in isolation** (`-Parallel 1`, single process, single class) — not a
parallel-run contention artifact.

Classes observed to crash this way:

```text
org.elasticsearch.index.mapper.TsidExtractingIdFieldMapperTests
org.elasticsearch.index.mapper.UpdateMappingTests
org.elasticsearch.index.query.CombineIntervalsSourceProviderTests
org.elasticsearch.index.query.DisjunctionIntervalsSourceProviderTests
org.elasticsearch.index.query.FilterIntervalsSourceProviderTests
```

(`UpdateMappingTests` is also named in the original crash doc's "Examples"
list — it crashed there too, but for the *old*, now-fixed reason. It crashes
again today for one of the reasons below.)

## Crash signatures (symbolized against the same `release-with-debug` binary)

Four distinct faulting sites across the 5 classes, all in the
JIT/deopt/GC-tracking machinery:

```text
0x1E5B16  hashbrown::map::HashMap::get<String, Vec<cratonvm_jit::deopt::DeoptEvent>, ...>
          [jit/src/deopt.rs — DeoptimizationLog::history/deopt_count, via hashbrown map.rs:1311]
0xB9060D  cratonvm_vm::jit::helpers::DeoptimizationController::deoptimize+0x88D
          [vm/src/jit/helpers.rs:4662]
0x332D13  cratonvm_jit::JitCache::get+0x93
          [jit/src/lib.rs:3990]
0x2378FC  cratonvm_gc::vm_heap::VmHeap::flush_thread_satb+0x4C
          [gc/src/vm_heap.rs:978]
```

`DeoptimizationLog::history`'s backing `FxHashMap` **is** Mutex-guarded
(`SharedVm::deopt_log: parking_lot::Mutex<DeoptimizationLog>`,
`vm/src/vm/vm_init.rs:614`) — the crash is not a plain missing-lock data
race on that map. The crashing thread is always a non-main thread
(`Thread-3`, `Thread-14`, `Thread-19`, `Thread-20` across different runs),
consistent with a JIT background-compiler or randomizedtesting worker thread.

The spread across deopt-log / deopt-controller / JIT-code-cache / GC
SATB-buffer-flush sites — rather than one single crash site — suggests
either (a) several independent JIT/GC memory-safety bugs in this area, or
(b) one earlier heap/pointer corruption that manifests wherever the
corrupted memory is next touched (the classic "different stack trace each
run" heap-corruption signature). `DeoptimizationController::deoptimize` was
the site of a previously-documented (and supposedly fixed) JIT-code
use-after-free — `deoptimize -> jit_cache.remove -> ExecutableBuffer::drop ->
VirtualFree` while other compiled methods still hold direct-call sites into
the freed buffer (see `reference_crash_debug_tooling` session memory /
`docs/real-raf-segv-root-cause.md`) — this may be a regression of that same
class of bug, or a related-but-distinct UAF along the same code path. Not
confirmed; needs a fresh investigation, not assumed.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 1689 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName jit-deopt-gc-repro `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir <workdir>\apps\elasticsearch-suite-runner\.suite `
  -Exe <path-to-cratonvm.exe>
```
(`-Start 1689` selects `org.elasticsearch.index.mapper.UpdateMappingTests` in
this repo's current `all-tests.tsv` snapshot — re-verify the index if the
class list has been refreshed since.)

Build with `--profile release-with-debug` (keeps line-tables, doesn't strip)
to get symbolized crash reports from the VEH; then symbolize the printed
`faulting RVA` against the SAME binary:
`CRATONVM_SYMBOLIZE=0x<rva1>,0x<rva2>,... <exe> X`.

## Evidence

```text
C:\craton\CratonVM-es-mapper-query-merge-crashes-20260704\apps\elasticsearch-suite-runner\.suite\results\es-mapper-fullbatch-20260704\all-jit\results.tsv
C:\craton\CratonVM-es-mapper-query-merge-crashes-20260704\apps\elasticsearch-suite-runner\.suite\results\es-mapper-p1recheck-20260704\all-jit\logs\ (per-class .err.log files carry the full VEH crash report + register dump)
```
