# jit review

Crate: `cratonvm-jit` at `C:\Projects\CratonVM\jit` — the x86-64 JIT compiler
(plus a partial aarch64 backend). 18 source files, ~53 k lines (x64.rs alone
~25 k). Heavy `unsafe`: raw VirtualAlloc/mmap pages, custom encoder, fn-ptr
`transmute`, raw `*const u8` metadata embedded in emitted code.

## Summary

- **HIGH** — Linux/FreeBSD aarch64 builds **never invalidate the instruction
  cache** after JIT-emitted writes, even though the README documents this as
  required. Only macOS-aarch64 calls `sys_icache_invalidate`. Stale icache
  lines mean undefined execution of compiled methods on every non-Apple ARM64
  host. (`platform.rs:322`-`353`.)
- **HIGH** — Untrusted-bytecode integer overflow in `tableswitch` /
  `lookupswitch` length math: `(high - low + 1).max(0) as usize` wraps in
  release. Triggers in `regalloc.rs:74`, `x64.rs:1237`, `x64.rs:1495`,
  `x64.rs:9463`, `x64.rs:11617`. Negative `npairs` from `lookupswitch`
  becomes a huge `usize` (`regalloc.rs:82`, `x64.rs:9477`). Defence-in-depth:
  `cratonvm-reader` may validate, but the JIT should not trust it.
- **HIGH** — "JIT never panics" contract (declared in `x64.rs:7-34` and the
  `#[deprecated]` shims at `lib.rs:482`) is **broadly violated**: 43 callers
  of the panicking `patch_i32`/`patch_byte` shims in `x64.rs` + 1 in
  `ir_lower.rs`; zero callers have migrated to `try_patch_*`. Production
  `assert!` (not `debug_assert!`) at `x64.rs:8941/8945/8985`, `ir_lower.rs:78`,
  `lib.rs:855/911/1271`. Production `.unwrap()`/`.expect()` at
  `x64.rs:13997/14095/14236/14389/14479/15139/15429`.
- **MED** — `JitInvokeInfo.class_name: &'static str` is built via
  `unsafe { &*class_ref }` from a `Box<str>` whose real lifetime is the
  `CompiledMethod._jit_strings` vector (`lib.rs:3766-3786`). The `'static`
  annotation is a lie; sound only because the box is moved into the
  `CompiledMethod` that owns the emitted code, and that ownership is
  enforced by convention, not by the type. `compile()` also takes raw
  `*const u8` / `*const JitInvokeInfo` / `*const JitMICSlot` in its public
  signature with no `unsafe` keyword.
- **MED** — Encoder has **no bit-pattern / differential-decoder tests**.
  Every JIT test is end-to-end execution; a wrong byte that still
  decodes to a semantically-equivalent instruction would pass. No fuzzing
  of the encoder, no cross-check against an x86-64 disassembler. Coverage
  in tests/ (8 files, 86 tests) and src/ (681 `#[test]`) is excellent for
  behavioural correctness, weak for encoder rigour.

## 1. Code review

### Bugs

- **HIGH — Missing icache flush on Linux/FreeBSD aarch64.** `platform.rs:236`
  calls `sys_icache_invalidate` only for `target_os = "macos"` + aarch64. The
  generic Unix path (`platform.rs:321-336`) does only `mprotect(PROT_READ|PROT_EXEC)`.
  ARM64 mandates a `dc cvau; dsb ish; ic ivau; dsb ish; isb` (or
  `__builtin___clear_cache`) sequence; without it, freshly emitted code can
  execute stale icache bytes — silent miscompile or SIGILL. README
  (`aarch64.rs:29`) acknowledges the requirement but the platform code does
  not honour it on Linux ARM64.

- **HIGH — Switch-length overflow on adversarial bytecode.**
  - `regalloc.rs:74`: `(high - low + 1).max(0) as usize` — `i32` subtraction
    wraps when `high - low > i32::MAX`. The result feeds
    `p + 12 + count * 4` (line 75) and can land out of bounds.
  - Same pattern in `x64.rs:1237`, `x64.rs:1495`, `x64.rs:9463`, `x64.rs:11617`.
  - `regalloc.rs:82`/`x64.rs:9477` cast `npairs: i32` (lookupswitch) to
    `usize` with no non-negative check; a negative i32 becomes ~2^63 → the
    subsequent `npairs * 8` overflow / out-of-bounds.

- **HIGH — Production panics in the codegen hot path.**
  `x64.rs:8941-8989` uses `assert!` (release-enforced) to bound `rel8`
  displacements emitted by `emit_safe_idiv`. Comment justifies the assert
  as "intervening block is fixed-size and small", yet `assert!` defeats
  the "bail to interpreter on codegen error" contract advertised in
  `x64.rs:7-34`. `ir_lower.rs:78` panics on spill-frame overflow.
  `lib.rs:855/911/1271` panics via `.expect()` if `validate_code_ptr` ever
  rejects the entry — i.e. a JIT-cache bookkeeping bug aborts the VM
  instead of falling through.

- **HIGH — Deprecated panicking patch shims still in use.** `lib.rs:482-491`
  marks `patch_i32`/`patch_byte` `#[deprecated]` with an explicit note that
  these "violate the JIT never-panic contract". Yet `x64.rs` has 43 call
  sites and `ir_lower.rs` has 1; no caller has migrated to `try_patch_i32`/
  `try_patch_byte`. A stale recorded patch site (post-overflow path) will
  panic the VM.

- **MED — `JitCache::call()` silently returns 0 for >8 args.** `lib.rs:898`,
  `lib.rs:950`. A method whose JVM signature has more than 8 i64-sized
  arguments is silently miscalled: stderr-printed warning, JIT entry not
  invoked, caller receives 0. The actual JIT prologue supports stack-passed
  args; only the wrapper API is the limitation. Real correctness bug for any
  method with >8 args invoked through `CompiledMethod::call`.

- **MED — `'static` lifetime aliased into JIT-owned `Box<str>`.**
  `lib.rs:3766-3786`. `JitInvokeInfo { class_name: &'static str, ... }` is
  populated via `unsafe { &*(class_ref: *const str) }` where `class_box` is
  moved into `owned_strings` and the boxed contents are referenced via raw
  pointer baked into emitted code. The `'static` annotation is unsound (the
  data is tied to the CompiledMethod's lifetime); only the by-convention
  `_jit_strings` field on `CompiledMethod` keeps it alive. Recommend
  `class_name: *const str` or `Arc<str>`.

- **MED — `JitMICSlot::update` is not atomic with respect to
  `cached_class_name`.** `lib.rs:2205-2212`. The lock-write of
  `cached_class_name` sits between two atomic stores; concurrent readers
  on a populating cache can observe a `class_id`/`entry_ptr` pair while
  the name is still being mutated. The JIT-emitted fast path doesn't
  read the name, so the data race is benign in practice — but it is
  technically a data race per the Rust memory model unless the mutex
  is the synchronization point.

- **MED — Doc-comment drift.** `lib.rs:290-292` claims "the memory is
  always RWX and `finalize` is a no-op" on Windows / Linux x86-64. False —
  `platform.rs` allocates RW and `finalize` does `VirtualProtect(PAGE_EXECUTE_READ)`
  / `mprotect(PROT_READ|PROT_EXEC)` (correct W^X) on every platform. The
  doc-comment misleads anyone auditing the W^X story.

- **MED — README `compile_or_get` does not exist.** `README.md:32` shows
  `cache.compile_or_get(...)` but the only public compile entry is
  `try_compile()` (`lib.rs:3230`). `JitCache` exposes `get`/`put`/`remove`
  only.

- **LOW — Off-by-one in `modrm_rbp_disp`'s disp8 range.** `x64.rs:4193`
  uses `(-127..=128).contains(&disp)` — correct given the negation, and
  documented (`x64.rs:4186-4191`). Mirrored in
  `emit_movq_mem_rbp_from_xmm` / `emit_movq_xmm_from_mem_rbp`
  (`x64.rs:4282/4311`). Encoder is correct here; flagging because the
  off-by-one of the previous version was a real bug, and the surrounding
  files have many ad-hoc disp checks elsewhere (e.g. `x64.rs:4642`'s
  `(-128..=127)` for positive disp) — divergent conventions invite
  future regressions. A single helper would close the gap.

- **LOW — Intel branch hint prefixes `0x3E`/`0x2E` (`x64.rs:3530`) are
  ignored on every CPU since Sandy Bridge; emit-time cost (bytes) without
  micro-arch benefit.

### Vulnerabilities

- **Untrusted-bytecode integer overflow** (see HIGH above) — the most
  exploitable surface. A maliciously-crafted class file with a
  `lookupswitch` of `npairs = -1` could craft an arbitrary out-of-range
  bytecode read in the JIT compiler (DoS or info leak via panic message).

- **No ROP-gadget surface in the user-controlled operand space.** The
  emitted code embeds attacker-controlled values only as: (a) class_id
  immediates (u32), (b) field offsets (u8/u32 — derived from
  field_index, not from the class file directly), (c) constant pool
  numeric literals (ldc/ldc2_w, sign-checked via i64). None are
  executed; all are operands of MOV/CMP/ADD. The actual code bytes are
  generated by the JIT itself — no path lets an attacker insert
  arbitrary bytes into the emitted stream. Good.

- **Code-region validator** (`lib.rs:166-180`) is sound: O(log n)
  binary search against registered allocations, called from `call()` and
  `call_with_context()` and the OSR trampoline path. Drop-time
  deregistration (`lib.rs:548-552`) plus the OSR-trampoline-cache prune
  (`lib.rs:707-714`) prevent dangling-pointer reuse after method
  invalidation. **No issues.**

### Stubs

`grep -E "(todo!|unimplemented!|FIXME|XXX|HACK)"` finds only documentation
references to those macros — no live stubs in production code. The crate
is feature-complete for what's wired in. Several `TODO(round-N+)` markers
exist (e.g. `lib.rs:2680`, `lib.rs:2729`, `loop_analysis.rs` module doc)
flagging deferred work (lock-free JitCache; arena-backed `ExecutableBuffer`;
generic LICM hoist consumer). The `deopt.rs` module is well-tested but
the actual runtime hook that resumes the interpreter from a `DeoptimizationPoint`
appears to live in the VM, not here — `deopt.rs` is metadata-only.

### Performance

- **Code-cache fragmentation** — every compile calls `VirtualAlloc` on
  Windows (64 KiB granularity per ~5 KiB method = ~92% waste). `lib.rs:2729-2770`
  spells out the round-8 arena rework plan; not done. ~177 MB of reserved
  VA tax on a typical JDK workload.
- **`JitCache` is wrapped in `parking_lot::RwLock` at the VM level**
  (`lib.rs:2680-2727`). Every interpreter dispatch acquires the read lock;
  hot. Migration to `ArcSwap` / 16-shard is deferred (documented).
- **`JitCache::get` does its own u64-hash + key-verify** (`lib.rs:2816`) —
  branch-free, no Arc clones. Good.
- **CPU feature detection cached via `AtomicU8`** (`x64.rs:121-269`) —
  one CPUID per process for AVX2, SSE4.1, SSE4.2, POPCNT, LZCNT, BMI1,
  PCLMULQDQ. Correct.
- **OSR trampoline cache** (`lib.rs:1022-1294`) — keyed by `target_addr`,
  emitted-once-per-PC, `Arc<ExecutableBuffer>` keeps body alive. Good.
- **`flush_scratch_registers` / `pop_to_rax`** — these are the workhorses
  in the codegen ladder; no obvious peephole gaps in the x64 emitter, but
  the IR pipeline (`ir_optimize.rs`) currently runs only constant fold,
  algebraic simplify, GVN, DCE in an 8-iteration fixed point — no
  inlining decisions, no loop-invariant code motion at the IR level
  (LICM is done on bytecode upstream of the IR), and the IR is only
  used for "simple integer-only methods" (`lib.rs:3406`). The bulk of the
  codegen falls back to the linear `x64::compile` path.

## 2. Tests

### Coverage

- **767 tests total**: 681 `#[test]` in `src/` (180 in `x64.rs`, 84 in
  `aarch64_backend.rs`, 82 in `lib.rs`, 62 in `pgo.rs`, 49 in `deopt.rs`,
  40 in `tiered.rs`, …) + 86 in `tests/` (mostly intrinsic differential
  suites: arraycopy, arrays_ops, arrays_sort, crc32, int_bits, long_bits,
  string_access, string_search). Behavioural coverage is excellent and
  meets the ≥85% bar by file count.
- **`differential.rs`** runs FP-loop bytecode through the JIT and checks
  bit-exact against host f64/f32 reference. **Real differential testing
  exists, but only for FP arithmetic.**
- **Intrinsic tests** assert bit-exact equality with Rust reference impls
  (`u32::count_ones`, `i32::leading_zeros`, …). High quality.

### Gaps

- **No encoder bit-pattern tests.** Every test runs the emitted code; no
  test compares emitted bytes against an authoritative encoding (e.g.
  `iced-x86` disassembler, NASM output, or hand-checked reference
  sequences). A bug that produces a different-but-semantically-equivalent
  encoding goes undetected.
- **No proptest / fuzz target for the encoder.** Given 25 k lines of
  bit-manipulation, a property test ("for every (reg, disp) in range,
  decode-and-re-encode is a fixed point") would catch most encoder regressions.
- **No tableswitch / lookupswitch adversarial-input tests.** The integer
  overflow bugs flagged above (HIGH) would fall to a single fuzz target.
- **No OOM-on-code-cache test.** `ExecutableBuffer::new(usize::MAX / 2)`
  should return `None`, but no test exercises this path.
- **No `try_patch_i32` test that the never-panic path actually works in
  context.** Lib tests exist, but no codegen test exercises it.
- **No deopt end-to-end test.** `deopt.rs` has 49 unit tests of the
  metadata types, but none assert that a deopt point actually steers the
  interpreter back at the right BCI with correct frame state — because
  the runtime hook lives in the VM. This is OK for the crate boundary,
  but the *crate's docstring* (`lib.rs:55-72`) is precise about the
  GC-safety contract; no test asserts the conservative-frame-sweep
  invariant is met by the emitted code.
- **No race/thread-safety test of `JitMICSlot::update`.** The known data
  race (MED) goes unobserved.

### Concrete additions

1. Add `tests/encoder_bitpattern.rs` that JIT-compiles canonical
   ld/store/branch sequences and `assert_eq!`s the emitted bytes against
   reference encodings. Include disp8/disp32 boundary cases.
2. Add `fuzz/fuzz_targets/jit_switch.rs` (the workspace already has
   `fuzz/`) generating `tableswitch` / `lookupswitch` byte streams with
   adversarial `low`/`high`/`npairs` — assert no panic, no OOB read.
3. Add `proptest`-based round-trip: `for (reg, disp) ∈ valid_range,
   decode(encode(MOV reg, [rbp-disp])) == MOV reg, [rbp-disp]`. Use a
   third-party disassembler.
4. Add `tests/code_cache_oom.rs`: request a buffer so big platform-alloc
   fails; verify `ExecutableBuffer::new` returns `None` and the compile
   driver bails to interpreter.
5. Add a `loom`-based test of `JitMICSlot::update` vs. lookup to surface
   the cached_class_name race.
6. Add a Linux-ARM64 CI lane (currently the icache-flush bug is
   un-tested because the host CI presumably runs x86-64).

## 3. Documentation

### Existing

- **Crate-level rustdoc** in `lib.rs:4-72`: excellent. Documents the
  internal calling convention (FP-in-GPR), the W^X story (claim
  contradicts code, see MED above), and the precise/conservative
  GC-safepoint contract.
- **Per-module rustdocs** on every file. Architecture (`x64.rs:36-61`),
  ABI (`x64.rs:36-43`), stack layout (`x64.rs:49-61`), tiered-compile
  table (`tiered.rs:7-15`), W^X platform abstraction (`platform.rs:5-16`).
- **`OopMapEntry` and oop-map invariants** (`lib.rs:586-614`,
  `lib.rs:776-832`) — thorough.
- **README** (`README.md`): scope / non-goals / status / licence — clean.

### Missing

- **README claim `cache.compile_or_get(...)` is fictional** (see MED bug).
- **W^X claim** in `lib.rs:290-292` contradicts `platform.rs` (see MED).
- **No top-of-crate diagram** of the pipeline. The pipeline is
  `bytecode → jit_scan → ir? → ir_optimize → ir_schedule → ir_lower →
  CompiledMethod` *with a fallback* `bytecode → jit_scan → x64::compile`.
  Both paths are dual-implemented; the README does not say which methods
  go which way (the answer is `ir.rs::ir_compatible(&scan)` — integer-only
  methods).
- **No safety summary for the `compile()` public API** (`x64.rs:16200`),
  which takes raw pointers in its parameter list (`Vec<(usize, *const u8, usize)>`,
  `Vec<(usize, *const JitInvokeInfo)>`, `Vec<(usize, *const JitMICSlot)>`,
  `Vec<(usize, *const JitPICSlot)>`) but is not marked `unsafe fn` and has
  no `# Safety` doc-section. Callers (the VM) must ensure these pointers
  outlive the resulting `CompiledMethod`; this is enforced only by
  attaching `Box`es to `_jit_strings`/`_jit_invoke_infos`/`_jit_mic_slots`/
  `_jit_pic_slots` on the return value — entirely by convention.
- **No `docs/` directory inside the crate**. Workspace-level docs may
  contain more (`docs/roadmap.md` is referenced from `lib.rs:691`,
  `lib.rs:1602`, `lib.rs:1944`) — those should be linked from the
  crate-level rustdoc.
- **ASCII art** in lib.rs and x64.rs is fine; no rendered diagrams. For
  a 25 k-line codegen file this is a maintainability risk.

## 4. OSS readiness

- **`Cargo.toml`**: `description`, `readme`, workspace-inherited
  `license`/`repository`/`keywords`/`categories`. **OK.** `vm-tests`
  feature is declared but unused inside this crate — fine.
- **SPDX headers** present on every `.rs` file (verified by `grep -L`).
- **Copyright** consistent ("Craton Software Company", 2024-2026).
- **`publish = false`** workspace-wide; `Cargo.toml` here does not
  override. Crate is intentionally **not** for crates.io publication
  in this iteration.
- **NOTICE** at workspace root is minimal but correct (Apache-2.0).
- **MSRV** inherited from workspace (1.77). No `rust-version`
  pin override. Code uses `core::mem::offset_of!` is avoided
  explicitly because MSRV 1.75 was the prior bar (`lib.rs:2135`) —
  comment is out-of-date if MSRV is now 1.77.

### Blockers for publication

If the workspace ever flips to `publish = true`:

1. The `'static`-aliasing in `JitInvokeInfo` (MED) must become a
   proper lifetime or `*const str`. Public API hands out an unsound type.
2. Public `compile()` takes raw pointers without `unsafe fn` —
   undefined behaviour if a downstream user calls it directly. Mark
   `unsafe fn` and add a `# Safety` section.
3. README's `cache.compile_or_get(...)` example must compile.
4. The deprecated `patch_i32`/`patch_byte` shims should be removed
   (every caller migrated to `try_patch_*`), as their own deprecation
   note says.

### Not blockers

- 0 dependencies outside the workspace except `parking_lot` and
  `rustc-hash` — light, well-known, Apache/MIT.
- No `build.rs` doing unsafe code-generation.
- No external assembler / LLVM bindings.

## Top 5 fix priorities

1. **(HIGH)** Add `__clear_cache` / `cacheflush` call in the generic
   Unix `platform_make_executable` path (`platform.rs:322`) so Linux/FreeBSD
   aarch64 builds do not execute stale icache lines.
2. **(HIGH)** Switch `(high - low + 1)` and `npairs` cast in
   `regalloc.rs:74/82`, `x64.rs:1237/1495/9463/9477/11617` to
   `i64`-checked arithmetic with bounded-failure return. Adversarial
   bytecode must not panic / wrap / OOB-read.
3. **(HIGH)** Migrate every caller of `patch_i32`/`patch_byte` to
   `try_patch_*`; promote `assert!`s in `x64.rs:8941/8945/8985` to
   "bail to interpreter" returns; honour the documented never-panic
   contract crate-wide.
4. **(MED)** Replace `&'static str` in `JitInvokeInfo`
   (`lib.rs:1440-1447`) with a sound lifetime carrier; mark
   `x64::compile`'s raw-pointer parameters in an `# Safety` rustdoc
   block (or wrap them in a typed view).
5. **(MED)** Add encoder bit-pattern tests + `tableswitch`/`lookupswitch`
   fuzz target; CI a Linux-aarch64 lane. Together these would have caught
   priorities (1)-(2) automatically.
