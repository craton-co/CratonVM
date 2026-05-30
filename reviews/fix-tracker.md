# CratonVM fix orchestration tracker (branch: dev)

Pool of up to 9 parallel Opus agents per wave. Each item owns a disjoint file set.
Agents write code only (no build). Orchestrator builds + fixes + commits between waves.

Legend: ⬜ queued · 🔄 in wave · ✅ landed+built · ⏭️ deferred

## Wave 1 — CRITICAL / HIGH (code)
- 🔄 W1-awt-uaf      — native-awt/src/edt.rs, native-awt/src/natives.rs (EDT runnable UAF: GC-gen gate)
- 🔄 W1-jit          — jit/src/x64.rs, jit/src/ir.rs, jit/src/platform.rs (truncated-bytecode OOB; checked tableswitch; expect→bail; protect diag)
- 🔄 W1-coll         — native-collections/src/lib.rs (access-order LHM put; stream wrapping; listiterator noop; treemap unwrap; al_ensure_capacity None)
- 🔄 W1-gc           — gc/src/heap.rs, gc/src/gen_heap.rs (zero-init Object(None) regression; unused_unsafe)
- 🔄 W1-cuda-jit     — jit-cuda/src/lowering/emit.rs, jit-cuda/src/analyzer.rs (i2c zero-extend; getfield/estimated_work; pN_len cache)
- 🔄 W1-cuda-bridge  — cuda-bridge/src/launch.rs, backend_cuda.rs, lib.rs (stream launch; dup event; dead code; intern cap; assert msg)
- 🔄 W1-types        — types/src/compact_value.rs (degradation diagnostic + hardened docs; NO breaking API rename)
- 🔄 W1-awt-render   — native-awt/src/graphics2d.rs, peer.rs, win32.rs, renderer.rs, font.rs (save composite; peer recursion→stack; measure_text chars; fill_polygon total_cmp; LRU)
- 🔄 W1-vm           — vm/src/jit/helpers.rs, vm/src/runtime/value_stack.rs (jit_newarray try_alloc OOME; unwrap_or(0) fix)

## Wave 2 — MEDIUM (code)
- ⬜ W2-nb-panama    — native-builtins/src/panama.rs (setUtf8String zero-size; gate get/set/fill)
- ⬜ W2-nb-unsafe    — native-builtins/src/lib.rs (Unsafe getAndSet/getAndAdd index bounds)
- ⬜ W2-nb-asn1ser   — native-builtins/src/jca/asn1.rs, native-builtins/src/serialization.rs (OID overflow; JEP-290 limits)
- ⬜ W2-nb-misc      — native-builtins/src/unsafe_natives.rs, native-builtins/src/zip_real.rs (arena freed mask; direct-bb inflate note)
- ⬜ W2-nio-native   — native-io/src/nio_native.rs (range_len i64::MAX; read0/write0 size cap)
- ⬜ W2-nio-dbb      — native-io/src/direct_buffer.rs (ABA double-free generation)
- ⬜ W2-nio-ssrf     — native-io/src/outbound_policy.rs, native-io/src/socket_channel.rs (custom-policy on resolved IPs)
- ⬜ W2-nio-tests    — native-io/src/lib.rs (confinement test isolation; stale FIS/FOS registration asserts)
- ⬜ W2-jfr          — jfr/src/* (test isolation; delta-ts underflow; threshold cast; builtin validation; file-size cap; active_recording_count)

## Wave 3 — MEDIUM/LOW (code)
- ⬜ W3-vmcli        — vm-cli/src/main.rs, vm-cli/tests/cli_main_args.rs (-Xms VALUE_TAKING_OPTS; watchdog cancel; stale doc; cause-chain cap marker)
- ⬜ W3-reader       — reader/src/instruction.rs, byte_view.rs, attribute.rs, lib.rs (checked add; ByteView::new pub(crate); dispatch collapse)
- ⬜ W3-native-api   — native-api/src/fd_table.rs, ffi.rs, native_ring.rs (path contract docs; align_up overflow; ring gating)
- ⬜ W3-classloading — classloading/src/class_path.rs, verify_insn.rs (percent-decode; resource filter align; multianewarray dim>=1)
- ⬜ W3-jit-api      — jit-api/src/lib.rs (const usize==8 assert; gpu-lowering decision)
- ⬜ W3-craton-gpu   — craton-gpu/build.rs (rerun-if-changed guard)
- ⬜ W3-numa-oldgen  — gc/src/numa.rs, gc/src/old_gen.rs (cpulist cap; free accounting)
- ⬜ W3-perf-jit     — jit/src/lib.rs (oop_map sorted flag) [serialize after W1-jit if same files — lib.rs disjoint from x64.rs ✓]
- ⬜ W3-fuzz         — fuzz/README.md, fuzz/Cargo.toml (target table + build.sh sync)

## Wave 4 — DOCS + OSS metadata (mostly non-code)
- ⏭️ W4-docs-crypto  — docs/CRYPTO_STATUS.md, SECURITY.md, CHANGELOG.md (reconcile crypto/SecMgr/zip-drift)
- ⏭️ W4-docs-mode    — README.md, docs/INSTALL.md (default-mode reconcile; CLI flag table)
- ⏭️ W4-docs-release — RELEASING.md, ARCHITECTURE.md (workflow status; IR claim)
- ⏭️ W4-oss-cargo    — root Cargo.toml + member Cargo.tomls (publish flags, homepage.workspace, exclude lists)
- ⏭️ W4-cleanup      — remove stray logs; .gitignore *.log; git rm --cached continue_prompt.md
- ⏭️ DEFERRED        — crate renames (jit-cuda→cratonvm-jit-cuda etc.) — invasive, touches every dependent manifest; do as a dedicated serial step last.

## Build log
(orchestrator appends cargo build results per wave here)
