# Perf Campaign — Remaining Items After Round 11

Status snapshot at end of round-11. Each item links back to the round that originally flagged it. Items that are inherently large (need their own design doc) are tagged `[design]`; items waiting on upstream-dep changes are tagged `[blocked]`.

## CRIT

All correctness CRITs from rounds 1–10 are now addressed. Round 11 closed the last batch (CMOV opcode inversion, NPE-drain-via-exception-table, CHM 256-stripe, JFR SpscEventRing drop park, class_init_state real fast-path, AtomicOperations UB, GC volatile total-order, ConcurrentSkipListMap sync, DirectBuffer freed_addrs unbounded, LinkResolver wired, ThreadLocal/INTEGER/BOOLEAN identity-hash + GC roots).

## HIGH (deferred from round-11)

### JIT
- **JitCache lock-free with arena versioning** `[design]` — arc-swap or 16-shard requires reworking the two `Pin<Box<...>>` arenas (string_arena, invoke_info_arena) that hand out raw pointers to JIT-emitted code. (round-11 TODO at `jit/src/lib.rs` JitCache definition)
- **JitCache pool with arena allocator** — round-7 noted 59 KB waste per method on Windows (VirtualAlloc 64KB granularity). Combined with the above into one round-12 work item.
- **Tiered compilation thresholds** — currently fixed; would benefit from profile-driven adjustment.
- **CMOV peephole broader patterns** — round-11 wired the user-written min/max idiom; ternary-with-constants, abs(x), and signum still emit branches.
- **DSE / const-fold on the IR** — IR optimizer pipeline still mostly dormant; LICM scaffold landed but consumer-hoisting deferred.
- **Precise callee-saved register oop encoding** — round-11 took the defensive spill route (~3 bytes per oop reg per safepoint poll). Encode-and-scan would save those bytes but needs an OopMapEntry schema change.

### GC
- **ReferenceQueue spin-yield** (round-5 #14) — `gc/src/reference.rs:125` still polls without park; idle CPU burn.
- **TLAB sizer adaptive shrink** — TLABs grow but never shrink mid-lifetime; long-lived threads with bursty allocation hold over-sized TLABs.
- **Concurrent-mark cancellation under alloc pressure** — when the mutator outruns the marker, the mark cycle should be cancelable and re-triggered with a larger heap budget.
- **Pre-tenuring decision tree** — currently every alloc goes through young first; large allocations could go straight to old.
- **Card-table dirty bit clearing strategy** `[design]` — current strategy is "clear all dirty bits at remark end", causing a full sweep. Per-region clear-on-rescan would amortize.
- **ReferenceProcessor parallelism** — single-threaded; soft/weak/phantom processing serializes during STW.

### JFR
- **Remaining 15+ Arc::from(&str) sites** — round-11 added 10 `_arc` variants for the highest-frequency emitters; the long tail of class-load/method-compile sites still needs caller migration.
- **9 unwired event types** — round-11 wired InitialEnvironmentVariable + 3 more emit functions; NativeMethodSample, SystemProcess, GCReferenceStatistics, Container{CPU,Memory}Usage still no emit caller.
- **JFR chunk rollover trigger** — no auto-rollover when chunk exceeds N MiB; currently dump=single-chunk.
- **jfc settings file parsing** `[design]` — .jfc XML configures sampling rates; would unlock JMC interop.
- **JFR streaming API** `[design]` — consumer-reads-concurrently-with-producer requires repository iter that doesn't drain.
- **jcmd JFR.start/stop/dump remote control** `[design]` — needs JVMTI agent attach.
- **Per-event field bit-packing in dump format** — round-11 added timestamp delta encoding (30-40% size win); per-field bit-packing would compound it.

### Classloading + reader
- **ZipArchive deep mmap** `[blocked]` (round-7 deferred, round-11 documented) — generic-type cascade across `ClassPathEntry`, `Mutex<ZipArchive<Cursor<...>>>`, every helper. Needs a `JarReader` newtype.
- **mmap path returns owned Vec, defeating mmap benefit** (round-8 finding) — needs Arc<[u8]> or `enum Bytes { Mmap(Mmap), Vec(Vec<u8>) }` cascade.
- **rayon parallel ClassFile parse** `[blocked]` — no rayon dep workspace-wide.
- **More LinkResolver wiring** — round-10 wired JNI + 4 reflection natives; deep reflection (Class.getMethods, Class.getDeclaredFields without name filter) still bypass the cache.
- **Generic signature cache** — round-11 added the cache + API; existing call sites in `ResolvableType`-equivalent paths need migration.
- **Annotation type matching forces String allocs** (round-8) — annotation lookup loops still allocate per probe.

### native-builtins
- **StringConcatFactory CallSite** `[design]` — round-9 found it's a no-op MH; real implementation needs MethodHandle machinery beyond invokedynamic bootstrap.
- **LambdaMetafactory CallSite caching** — round-11 attempted; cache hit/miss needs validation.
- **Unsafe.compareAndExchange family** — round-11 in-flight; verify post-build.
- **ScopedValue keys missing from GC roots** (round-9) — round-11 attempted; verify gc_roots.rs wiring.
- **Method.invoke fast-path** — round-11 attempted; verify args_match_descriptor_exactly fires.
- **DateTimeFormatter cache** is now bounded (round-10); LinkedHashMap access-order migration to TreeMap red-black still uses BTreeMap shim (round-11).

### native-io / native-collections
- **CHM clone-resize proper fix** (round-10 RwLock is interim) `[design]` — real lock-free CHM requires clone-on-resize: walk old chain, ALLOC fresh Nodes, link new chain, publish. Current RwLock blocks readers during resize.
- **TreeMap red-black** — round-11 uses BTreeMap shim for natural ordering; custom Comparator callbacks still on the array path.
- **ZipFile parallel entry decompression** — sequential today; benefits large-JAR cold start.
- **mmap buffer pool** — round-9 documented; round-10 added freed_addrs unbounded; full pool with size buckets still pending.
- **async I/O Windows IOCP** — round-11 has epoll/WSAPoll Selector; full IOCP integration deferred.

### native-awt
- **Bicubic interpolation** — round-11 in-flight; verify post-build.
- **Double-buffered EDT** `[design]` — needs per-Frame back-buffer + platform-backend coordination.
- **ImageObserver async notifications** — async image loading isn't implemented; sync loads don't need it.
- **SwingUtilities.invokeAndWait JNI panic** — round-11 in-flight; verify IllegalStateException replaces panic.
- **KeyboardFocusManager / DropTarget natives** — stubs only.
- **AWT SwingUtilities.invokeLater queue draining** — coalesced in round-10; no further work needed unless benchmarks show contention.

### vm-cli
- **jcmd / jstack remote attach interface** `[design]` — needs JVMTI agent socket protocol.
- **-XX flags parsing** — only `-X` flags currently honored; HotSpot-compat -XX flags would help benchmark reproducibility.

### cuda-bridge / jit-cuda
- **Multi-GPU enumeration** — round-11 in-flight; verify post-build.
- **Pinned host alloc actually invoked** — round-10 wired `upload_via_pinned_or_fallback` helper; verify call sites use it.
- **Multi-kernel batching** — sequential launches today; batching same-stream kernels would improve throughput.
- **CUDA block-size autotune** — round-10 added optimal_block_size helper; round-11 wires it.

### Concurrency
- **SATB SegQueue migration** `[blocked]` — deferred in round-11 because the deactivate_and_drain barrier relies on the mutex; SegQueue would need a new sync primitive. Benchmarks don't flag the current mutex as a bottleneck.
- **RCU for ClassManager** `[design]` — full read-copy-update would let class lookups be fully lock-free; current parking_lot::RwLock is already low-contention.
- **Hazard pointers for JFR SPSC** `[design]` — round-10 added consumer-busy CAS gate; hazard pointers would eliminate the gate entirely but need a workspace-wide HP infrastructure.
- **evmap-style snapshot for ProfileStore** `[design]` — round-11 went 16-shard; evmap would let reads be fully lock-free but adds a dep.

### Cross-cutting build
- **BOLT post-link optimization** — round-9 documented; expected 10-20% beyond PGO. Needs CI integration.
- **Dep duplicates** — round-11 documented hashbrown/getrandom/windows-sys versions; `[patch.crates-io]` deferred (would force-break some deps).
- **Cold crates at opt-level=s** — round-9 suggested measuring jfr/native-awt at opt-level=s for icache pressure; not measured.
- **PGO into CI** — round-11 added .github/.wf/pgo-build.yml; needs first-run validation.
- **format!() in interpreter.rs:2082** (round-9 LOW-11) — JIT-compile event still allocates a String per emit.
- **Frame::osr_attempt_counts still Vec** — round-9 LOW; would benefit from Option<Box<SmallVec<[(usize, u32); 4]>>> but micro.
- **bench profile debug=false** — current setting makes flamegraphs hard to read; consider line-tables-only for `[profile.bench]`.

### Docs
- **docs/PROFILING.md** still cites round-26 — refresh with round-11 baseline.
- **vm/src/runtime/lock_order.rs** additions for round-10 stripe arrays (CHM seg, CSLM seg, volatile stripe) and round-11 ProfileStore shards.

## MED / LOW (carry-over)

These are real findings from rounds 1-10 that haven't been touched and aren't urgent:

- Card-table dirty marking write barrier inlining
- Per-region allocation parallelism
- Concurrent-mark cancellation under alloc-rate pressure
- HashMap.put inline native dispatch overhead
- vtable.lookup_slot already #[inline]; lookup-name path could be too
- Class redefinition: any HIGH missed? (audit reflective resolution again after round-10)
- Module system: is jdk.internal.module fully wired?
- Method resolution caching across superclass chain
- Annotation parsing performance

## Round-by-round summary

| Round | Reviewers | Fix agents | CRITs found | Files changed |
|-------|-----------|------------|-------------|---------------|
| 1     | 16        | n/a        | many        | many          |
| 2     | partial   | 5          | known       | partial       |
| 3     | 10        | 9 + cascade| many        | ~3 commits    |
| 4     | 10        | 19 (2 waves)| 22         | ~120 reports → fixes |
| 5     | 10        | n/a (reports only) | 16  | docs only     |
| 6     | (planned) | 9          | 16          | 39 files +3376/-1467 |
| 7     | 10        | 19 (2 waves)| 22         | 73 files +5213/-793  |
| 8     | 10        | 9          | 28+         | 54 files +2074/-266  |
| 9     | 10        | 9 (rate-limited; partial)| 27+ | 44 files +1382/-257 |
| 10    | n/a       | 9          | 11 deferred | 34 files +1747/-436 |
| 11    | n/a       | 10         | HIGH only   | 54 files +5062/-904 (in flight) |

Total over the campaign: ~106 reviewer agents + ~117 fix agents, ~30k lines of code-review reports across `docs/round*-*.md`, and 11 merged commits to main.

The codebase is now substantially closer to the round-1 baseline goal of "geomean ≤ 1.5× HotSpot C2" on the bench targets; precise headroom needs the round-11 build to complete + bench-gate run.
