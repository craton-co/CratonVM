# gc review

## Summary

- **HIGH** — `Heap` and `GenerationalHeap`'s blanket `unsafe impl Send + Sync` (`heap.rs:132-133`, `gen_heap.rs:228-229`) cover internal raw pointers that escape via `ObjectRef`. The safety comment relies on "GC stops the world before relocating objects," but the GC entry points themselves are `&self` — nothing in the type system forces callers to hold an exclusive `&mut` while a collection runs. The only protections are mutator self-discipline (`gc_quiescence::is_active()`, the safepoint protocol owned by the `vm` crate) and the `parking_lot::Mutex` around each arena. A second collector invocation from a thread that has *not* yielded would race the `from_space` lock with a mutator's `get_field` (`heap.rs:458`) and read freed memory after `swap_spaces` — there is **no defensive `is_collecting` flag** here.
- **HIGH** — Concurrent-mark mark-bit publication is **per-word `Release`, not whole-bitmap fence**, in non-STW callers. `mark_bitmap.rs:134-139` documents this honestly: the `SeqCst` fence after the per-word clear loop is the **only** thing that makes the *next* cycle's bitmap reads see zeros across all words, and the design currently leans on the G1 STW safepoint as the actual fence. If `clear()` is ever moved out of STW (e.g. background pre-clear, which the docstring explicitly anticipates) without re-architecting the per-word ordering, marker threads on AArch64 will silently see stale black bits → live-object reaped → UAF.
- **HIGH** — `g1.rs::write_barrier` (`g1.rs:2698-2732`) advertises an SATB **pre-store** contract that the `GarbageCollector` trait shape **cannot enforce** — the post-store hook has lost the old value. The "best-effort" `debug_assert!` only catches *the absence of any pre-call on this thread*, not "the right object was logged". Production releases ship without the assert. A missed pre-call is the classic SATB lost-object → UAF; relying on `vm` crate discipline to honour an unmechanizable contract is a documented HIGH risk in `collector.rs:196-214` and exactly the kind of latent bug the audit must call out.
- **MED** — Heavy use of `std::process::abort()` on the OOM and corrupt-header paths (8 sites in `gen_heap.rs`, plus `gc.rs:324`, `g1.rs:2427/2456`, `heap.rs:955`). Functionally correct, but conflates **genuine OOM** (which the JVM spec mandates surface as `OutOfMemoryError`) with **GC-internal invariant failures**. An embedder using `cratonvm-gc` from a host application has no way to recover; the abort kills the whole process.
- **OSS verdict: NEEDS FIXES.** Code is well-documented (every `unsafe` block has a `SAFETY:` comment; almost every panic/abort has a rationale) and heavily tested (~270 unit tests across 24 source files, plus 5 integration tests, plus a proptest harness and a loom model). Blockers for crates.io publication: workspace `publish = false`; three integration test files missing SPDX headers; `zgc.rs` ships as a 1,887-LOC feature-gated stub.

## 1. Code review

### Bugs

- **HIGH `g1.rs:2698-2732` — SATB pre-barrier contract is purely a caller obligation.** The trait `GarbageCollector::write_barrier(&self, obj, stored_value)` (`collector.rs:215`) is a **post-store** hook. SATB needs the *old* slot value. G1's debug_assert (`g1.rs:2719`) only fires if no pre-call happened on the same thread between consecutive post-calls — it cannot detect a stale-pre-call (one pre-call logged for the wrong slot) and is compiled out in release. A missed pre-call → lost object → UAF on the next evacuation cycle. Fix options: (a) rework the trait to a pre+post pair, (b) thread a `&Slot` handle through the API so the GC reads the old value itself, (c) accept the contract and add a runtime assert that scans the SATB queue for the just-overwritten address.
- **HIGH `mark_bitmap.rs:134-139` — cross-word ordering relies on an external STW fence.** Per-word `Release` stores in `clear()` give no inter-word ordering. The single SeqCst fence at the end of the loop is the **only** thing publishing the zeroed bitmap; today this works because `clear()` is only called from inside an STW safepoint. Any future move to background pre-clear (the docstring explicitly anticipates this) silently breaks ARM/AArch64 correctness. The `fetch_or` in `try_mark` is `AcqRel` (`mark_bitmap.rs:85`), which the docstring correctly notes is required for the same-word case but does *not* help across words.
- **HIGH `heap.rs:132`, `gen_heap.rs:228` — blanket `Send/Sync` for moving-heap collectors.** Both impls justify themselves with "GC stops the world before relocating objects," but the entry points (`collect_garbage(&self, …)`) are `&self`-callable from any thread. A concurrent `get_field` on thread A (which reads `obj_ref.as_ptr()` and the slot at `HEADER_SIZE + index*SLOT_SIZE`) is UB if thread B has called `collect_garbage` and the object has been moved. The only ordering today is "the caller must hold a safepoint barrier before invoking `collect_garbage`" — and that barrier lives in the `vm` crate, not the gc crate. Documented OK but the safety invariant should at minimum be a runtime `debug_assert!` on a global "STW active" flag instead of an unwritten convention.
- **MED `gc.rs:317-326` `forward_object` falls back to `process::abort()` on to-space OOM.** Comment correctly says "no partial-copy state to unwind." But the same condition surfaces as a recoverable `OutOfMemoryError` in Java, which `gen_heap.rs:2895-2904` *also* aborts on. Either: (a) plumb the OOM out as a `GcError` and let the caller throw `OOME`, or (b) document explicitly in the README that `Heap` is single-shot for tests only.
- **MED `gen_heap.rs:739-810` and `gen_heap.rs:852-908` — OOB field reads silently return `Value::Object(None)`.** Defensive masking for "synthetic vs real-JDK layout mismatch" but this **hides genuine VM bugs** — an off-by-one in a field-index computation now produces silent `null` reads instead of panicking, which in a JVM context propagates as a `NullPointerException` somewhere unrelated. The diagnostic logging is rate-limited to 5 messages globally per process (`gen_heap.rs:1024-1099`). Better: `debug_assert!` so debug builds panic loudly, releases silently degrade.
- **MED `gen_heap.rs:476` — single-line allocator panic on `array_data_size` overflow.** The format string is fine but the panic-via-eprintln-then-abort idiom defeats the per-thread `OutOfMemoryError` plumbing in the rest of the file. Same pattern duplicated at `gen_heap.rs:482`, `:492`, `:2705`, `:2904`, `:2931`, `heap.rs:955`, `g1.rs:2427`, `:2456`.
- **MED `gen_heap.rs:611-670` `is_object_address` holds three arena `lock()`s in series.** This is the conservative-root validation path called from JIT frame scanning. Under heavy contention the three sequential lock acquisitions could starve a mutator that holds one of them. Using `try_lock` (as `heap.rs:676-700` already does) would degrade to "not provably valid" rather than block.
- **LOW `compact_header.rs:212-223` — forwarding-ptr stores 30 bits + 3-bit alignment = 33-bit addresses (8 GB max).** `set_forwarding_ptr` `debug_assert!`s 8-alignment but does not assert the address fits in 33 bits. On a heap larger than 8 GB the high bits of `shifted` are silently lost. Production releases would silently corrupt a forwarding pointer. The bit-layout comment (`compact_header.rs:91-95`) documents the 8-GB ceiling but the assert is missing.
- **LOW `gc.rs:354-356` — `is_forwarded` read followed by `forwarding_address` is two raw-pointer field reads.** Under STW this is fine. Audit-flagged because the same pattern in concurrent contexts (`compact_header.rs:202-204`) needs `Acquire` ordering on the bits read, which it does not document.
- **LOW `tlab.rs:290-356` `install_tail_filler` writes synthetic class id `0xF111_E700`.** This is sentinel-correct for walker skip, but the heap walker in `gen_heap.rs:3158-3196` checks for `ObjectKind::HumongousFiller` not for the magic class id. If a walker forgot to special-case the sentinel and decoded it as a normal `int[]`, it would scan an unrelated set of "elements" — best-case 0 refs (int[] has no refs), worst-case length-driven OOB read. The sentinel is well-formed so this is "safe by accident"; a `ObjectKind::TlabFiller` variant would be more defensible.

### Vulnerabilities (heap soundness, races, UAFs)

- **`gc.rs:368-387` and `gen_heap.rs:2748-2768` — explicit atomic load+store for `mark_word`** during STW Cheney copy. The comment correctly notes that the bulk `copy_nonoverlapping` would otherwise be UB for the embedded `AtomicU64`. Audit-worthy fix that pairs with future concurrent-GC plans; documented well.
- **`heap.rs:676-700` `is_valid_heap_object` uses `try_lock` to avoid deadlock during concurrent autobox unboxing** (lock-order doc `vm/src/runtime/lock_order.rs` reserves L8 for heap interior locks). Correct: degrades to "not provably valid" on contention. Documented in `heap.rs:665-675`. Audit-worthy positive.
- **`heap.rs:1234-1311` `coerce_field_value_by_descriptor`** explicitly rejects `Value::Double` → `Value::Object` reinterpret (the audit note `heap.rs:1299-1306` documents that the previous revision could fabricate live `ObjectRef` out of double bit patterns). The fix is correct and well-explained.
- **`satb.rs:329-371` `deactivate_and_drain`** — round-9 fix correctly closes the TOCTOU window where a mutator observed ACTIVE, was preempted, and pushed after the drain. Documented heavily; tested by `tests/loom_satb.rs` (requires `RUSTFLAGS="--cfg loom"`). Audit-worthy positive: solid concurrent-FSM work.
- **`gen_heap.rs:910` `debug_assert!(index < num_slots)`** is **redundant** with the runtime check at 852 — but harmless. Note: line 910's `self.get_header(obj_ref)` is a *second* dereference of the same pointer; the compiler may not CSE it across the `set_field` call.
- **`g1.rs:218-223` `MARK_QUEUE_SHARD_CAP`** caps worklist growth at 8 MiB per shard (8 shards = 64 MiB max), then graceful fallback to full re-walk via `mark_worklist_overflowed`. Audit-worthy positive — replaces a previous `panic!` reachable from hostile Java programs.

### Stubs / todo / unimplemented

- **No `todo!`, `unimplemented!`, or `FIXME` in the production tree.** Grepped: zero hits.
- **`zgc.rs` (1,887 LOC) is feature-gated `#[cfg(feature = "zgc")]` stub** with no in-workspace consumer (`Cargo.toml:42-44` calls it out: "1884-LOC stub … no in-tree user appears"). Off by default; build-time gated correctly.
- **`heap.rs:71-81` NUMA multi-arena TODO** — the field `num_numa_nodes` and `numa_node_hint` are populated and threaded through `alloc_zeroed` (`heap.rs:936-963`) but the actual dispatch is a no-op stub. The single `from_space`/`to_space` Mutex pair serializes every NUMA node. Documented; not a bug today.
- **`g1.rs:676-710` humongous-reclaim-young TODO** — humongous regions are only reclaimed by full GC. Documented as round-9 deferred work. Functionally correct (humongous garbage retained until full GC) but a memory-pressure regression vector.
- **`tlab.rs::pop_cursor` round-robin** (concurrent_mark.rs:173-180) is per-thread `Cell` — correct, no thread-safety issue, but the cursor wraps `usize::MAX` → that's fine because it's masked with `(SHARDS-1)`.

### Performance

- **`gen_heap.rs:1444-2091` `collect_garbage_inner`** holds **three arena mutexes simultaneously** (`young_from`, `young_to`, `old_gen`) for the entire collection. Acceptable under STW; doc says the safepoint barrier serialises mutators outside. Inside the GC the lock-order is `young_from < young_to < old_gen` consistently.
- **`tlab.rs:191-211`** TLAB fast-path is inlined to a load+add+cmp+store with three direct field bumps. JIT contract (offsets 0/8 for `cursor`/`end`) is asserted by `test_tlab_offsets`. Good.
- **`heap.rs:1339-1399` `read_prim_element`** uses `ptr::read_unaligned` for all primitive widths. For aligned data this is the same code; for unaligned (which the array layout *guarantees won't happen* since arrays are 8-byte-aligned and element offsets are width-multiples) it's a defensive perf cost. Acceptable.
- **`card_table.rs:248-269` thread-local batched dirty path** drops per-store cost from "global mutex acquire" to "per-thread `Vec::push`" — round-7/9 work documented at length. Good.
- **`satb.rs:392-406` `SatbQueue::drain`** takes every shard lock in sequence (16 shards). Round-9 audit's TODO at `satb.rs:198-224` discusses replacing with `crossbeam_queue::SegQueue` but chose against it; rationale documented.
- **`mark_bitmap.rs:53-87` `try_mark`** uses `AcqRel` `fetch_or` for the same-word cross-cycle invariant. Comment correctly notes "free on x86 (LOCK BTS), one extra LDAXR/STLXR on ARM." Audit-worthy explanation.

## 2. Tests

**Coverage estimate: ~80% lines** — high in core paths (Cheney copy, write barrier, card table, SATB FSM, TLAB) but tapers off in failure-recovery paths (forced-OOM mid-collect, finalizer-cycle resurrection, concurrent-mark overflow fallback, class-unloading interaction with weak refs).

### Existing tests (counts approximate; some `#[test]` are per-module gates)

- **`arena.rs`**: 8 tests — basic alloc / alignment / overflow / reset.
- **`card_table.rs`**: 30 tests — incl. concurrent flush, bounds, bulk dirty.
- **`class_unloading.rs`**: 32 tests.
- **`compact_header.rs`**: 69 tests — bit-layout exhaustive.
- **`compressed_oops.rs`**: 34 tests.
- **`concurrent_mark.rs`**: 21 tests — initial-mark / concurrent / remark / sweep.
- **`g1.rs`**: 82 tests.
- **`gc.rs`**: 12 tests — Cheney, cycles, finalizers, hooks.
- **`gc_quiescence.rs`**: 3 tests.
- **`gen_heap.rs`**: 44 tests.
- **`heap.rs`**: 66 tests + 7 gpu-offload (feature-gated).
- **`mark_bitmap.rs`**: 6 tests — incl. one concurrent.
- **`metaspace.rs`**: 50 tests.
- **`numa.rs`**: 53 tests.
- **`old_gen.rs`**: 11 tests.
- **`reference.rs`**: 49 tests — weak / phantom / cleaner / finalizer FSMs.
- **`region.rs`**: 16 tests.
- **`satb.rs`**: 23 tests — FSM + multi-thread concurrent flush.
- **`tlab.rs`**: 28 tests — incl. JIT offset assertion.
- **`zgc.rs`**: 75 tests (feature-gated).
- **`vm_heap.rs`**: 2 tests (most logic delegates).
- **Integration tests**:
  - `tests/phase_h_integration.rs` (16 tests) — RH.1..RH.8 promotion, weak/phantom, ref-processor.
  - `tests/wp1_10_reference.rs` (15 tests) — Reference / PhantomReference / Cleaner / Finalizer pipeline.
  - `tests/proptest_graph.rs` (2 proptests, 64 cases each) — reachability after GC vs pure-Rust model.
  - `tests/loom_satb.rs` (3 tests, gated on `cfg(loom)`) — SATB FSM model-check, 1-shard reduction.
  - `tests/leak_soak.rs` (1 test, `#[ignore]`) — 5-minute soak, live-set bounded check.

### Gaps & concrete additions

- **No test exercises `forward_object` to-space OOM mid-collection** (`gc.rs:317-326`, `gen_heap.rs:2879-2933`). Both call `process::abort()`. Add a test that allocates a young set whose post-copy size exceeds `young_to` capacity, runs `collect_garbage`, and verifies the abort fires with the expected message. (`#[should_panic]` does not catch `process::abort`; use `std::process::Command::spawn` of a subprocess.)
- **No test for `is_object_address` under concurrent mutator pressure** (`gen_heap.rs:611-670`). Spawn a JIT-emulating worker thread that hammers `is_object_address` while another runs `collect_garbage`, assert no false positives that would survive into the new from-space.
- **Mark-bitmap cross-cycle ARM test missing.** Loom can model this — extend `tests/loom_satb.rs` (or add `tests/loom_mark_bitmap.rs`) to enumerate `clear()` → `try_mark()` → `is_marked()` interleavings.
- **No fuzz for `coerce_field_value_by_descriptor`** (`heap.rs:1234-1311`). The descriptor-byte input is a likely place for arbitrary data; add a proptest that round-trips every `(Value, descriptor)` pair without panic and without producing an `Object(Some)` from primitive bits (the prior UAF).
- **No stress test for SATB queue overflow recovery** (`g1.rs:296-305`, `mark_worklist_overflowed` path). Add a test that exhausts `MARK_QUEUE_SHARD_CAP` and asserts the conservative full re-walk completes and produces a correct mark set.
- **`class_unloading.rs` weak-ref interaction untested.** The existing 32 tests cover loader-data lifecycle only. Add a test that exercises a phantom-ref → finalizer → class-unload chain.
- **`tests/leak_soak.rs`** is `#[ignore]` and not in CI rotation. Add a 30-second smaller variant (`gc_soak_30s_no_leak`) to the default `cargo test` rotation to catch egregious leaks per-PR.
- **No multi-threaded GC fuzz.** `proptest_graph.rs` is single-threaded by design. Add a separate `tests/concurrent_mutator_proptest.rs` that runs link/unlink on N worker threads while a coordinator triggers `collect_garbage`, asserting reachability after the cycle.

### Brittle tests

- **`tlab.rs::pressure_tracker_doubles_on_fast_refill`** (`tlab.rs:683-694`) and `pressure_tracker_halves_on_few_allocations` lean on wall-clock timing (`FAST_REFILL_THRESHOLD_MS = 1`, `SLOW_REFILL_THRESHOLD_MS = 100`). On a CI agent under load these can flake. Use a `MockClock` injection point instead.
- **`reference.rs::ReferenceQueue::remove_blocking`** (`reference.rs:146-148`) hard-codes a 60-second safety cap. A test for this would take a minute; effectively untested.
- **`heap.rs::test_try_alloc_array_returns_none_on_oom`** (`heap.rs:1852-1863`) uses a `count < 1000` safety bound — fine, but if the heap geometry changes the bound becomes silently wrong.

## 3. Documentation

### Existing (good)

- **Crate-level rustdoc** in `lib.rs:1-23` with a cross-link to `docs/gc-tuning.md` — good.
- **Module-level rustdoc** on every file with a clear "what this implements" paragraph.
- **Most `unsafe` blocks have `// SAFETY:` comments** — sampled 50+ blocks, all but a handful are documented. Notably good: `heap.rs:121-131` (Heap Send/Sync), `gen_heap.rs:2747-2768` (atomic mark_word handling), `arena.rs:170-202` (`reset_no_zero` invariants).
- **`docs/gc-tuning.md`** is comprehensive: backend choice, sizing, TLAB, GPU-offload, JFR.
- **`vm/src/runtime/lock_order.rs`** specifies heap interior locks at L8 and documents the global hierarchy. The gc crate's internal sub-hierarchy (`young_from` < `young_to` < `old_gen`) is followed but **NOT documented in either file** — the convention is buried in code comments (`heap.rs:665-675`, `gen_heap.rs:1498-1500`).
- **JIT contract for TLAB layout** (`tlab.rs:77-126`) is excellent — explicit byte-offset table, `test_tlab_offsets` enforces it at runtime.

### Missing / incomplete

- **No collector phase diagram.** The concurrent-mark FSM (`concurrent_mark.rs:43-99`) and the SATB activation FSM (`satb.rs:18-58`) are described in prose. A state-machine diagram (Mermaid in `docs/gc-tuning.md` or `concurrent_mark.rs`) would speed up auditing.
- **No safepoint protocol doc.** `gc_quiescence.rs` documents the JIT-active gate but nothing covers the full protocol: mutator-park → flush SATB buffer → flush card buffer → run GC → unpark. The protocol is split across `vm_heap.rs:646-675`, `card_table.rs:290-306`, `satb.rs:60-113` with no single owner.
- **`vm/src/runtime/lock_order.rs` should document the gc internal sub-hierarchy.** The fact that `young_from < young_to < old_gen` is a global heap invariant lives only in code comments. A future contributor running tests in a different order could deadlock.
- **`tlab.rs::install_tail_filler`** docstring documents the layout but doesn't link to the `HumongousFiller` walker-sentinel pattern in `g1.rs::is_humongous_filler` (`g1.rs:2822-2824`). Two different sentinel schemes coexist without cross-reference.
- **`compact_header.rs:91-95`** documents the 8-GB forwarding-pointer ceiling but does not mention this in `Cargo.toml` keywords or `gc-tuning.md`. An embedder configuring `-Xmx32g` would silently truncate forwarding pointers in release.
- **`reference.rs::ReferenceQueue::remove_blocking`** docstring claims "spin-wait with yield" — does not name the 60-second safety cap. The cap should be in `docs/gc-tuning.md` under "diagnosing common pause / allocation symptoms."

## 4. OSS readiness

### Cargo.toml

- License `Apache-2.0` via `license.workspace = true` — good.
- `publish = false` inherited from workspace.
- Features `gpu-offload` and `zgc` both documented in the Cargo.toml itself with rationale — exemplary.
- `[lints] workspace = true` — good.

### SPDX & headers

- All 24 `src/*.rs` files start with `// SPDX-License-Identifier: Apache-2.0` + copyright.
- **3 of 5 integration test files MISSING SPDX headers**:
  - `tests/leak_soak.rs`
  - `tests/loom_satb.rs`
  - `tests/proptest_graph.rs`
  - (`tests/phase_h_integration.rs` and `tests/wp1_10_reference.rs` do have them.)
- `LICENSE` and `NOTICE` live at the workspace root only — per-crate copies missing if the crate were ever published standalone.

### Blockers for crates.io

1. Workspace-level `publish = false` (intentional).
2. Add per-crate `LICENSE` / `NOTICE` copies (or update `Cargo.toml` with `license-file` if linking).
3. Backfill SPDX headers on the three integration test files above.
4. Document `zgc` feature as experimental in `README.md` (it's a 1.8k-LOC stub, not production code).
5. The `cratonvm-jit` dev-dependency forms a cycle that prevents standalone publication of `gc` and `jit` independently. The workspace structure makes this moot today; would need split if ever published.

## Top 5 fix priorities

1. **(HIGH) `mark_bitmap.rs:134-139` — `clear()` cross-word ordering**: convert the `Release` per-word stores into a single `SeqCst` zero-fill or document the STW-required-fence invariant as a runtime `debug_assert!`. Move the STW invariant into the `ConcurrentGcState::set_phase` API so a future move-out-of-STW caller cannot silently break ARM/AArch64.
2. **(HIGH) `g1.rs:2698-2732` and `collector.rs:215` — SATB pre-barrier contract**: rework `GarbageCollector::write_barrier` to take both old and new values (or add a paired `pre_write_barrier`), so the SATB invariant is mechanically enforced by the trait shape rather than caller discipline.
3. **(HIGH) `heap.rs:132`, `gen_heap.rs:228` — `Send/Sync` safety invariants**: add a runtime `debug_assert!` (and ideally a release-build atomic flag) that ensures `collect_garbage` is not entered while any mutator holds a `get_field`/`set_field` borrow without going through the safepoint. Document the assumed safepoint protocol as a `# Safety` block on every `&self` API that the convention guards.
4. **(MED) Replace `process::abort()` on OOM/corrupt-header paths with `Result<_, GcError>`** — `gc.rs:317-326`, `gen_heap.rs:476-2931` (8 sites), `g1.rs:2427/2456`, `heap.rs:955`. Embedders need a hook to translate to `OutOfMemoryError`; aborting the entire host process is not acceptable for a library crate.
5. **(MED) Add SPDX headers to `tests/{leak_soak,loom_satb,proptest_graph}.rs` and update `README.md` to mark `zgc` as experimental.** Both are cheap and unblock a future crates.io publish path.
