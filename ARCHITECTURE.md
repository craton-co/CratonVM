# Architecture

This document describes the high-level architecture of CratonVM.
If you want to contribute, this is the place to start.

## Crate Layout

The workspace has 22 member crates (the `fuzz` harness is a separate,
standalone workspace, not a member):

```
cratonvm/
  reader/              cratonvm-reader              .class file parser
  types/               cratonvm-types               Shared types (Value, ClassId, ObjectRef)
  native-api/          cratonvm-native-api          Native capability facade & FD table
  native-builtins/     cratonvm-native-builtins     java.lang.*, registration & marshalling
  native-builtins-crypto/
                       cratonvm-native-builtins-crypto
                                                    Crypto compatibility kernels
  native-builtins-security/
                       cratonvm-native-builtins-security
                                                    JDK security & SunEC native pack
  native-collections/  cratonvm-native-collections  java.util.* native methods
  native-io/           cratonvm-native-io           java.io/nio native methods
  native-awt/          cratonvm-native-awt          AWT/Swing/Java2D native peers
  jit-api/             cratonvm-jit-api             JIT compiler API types
  jit/                 cratonvm-jit                 x86-64 / AArch64 JIT compiler
  jit-cuda/            cratonvm-jit-cuda            Java bytecode -> PTX lowering for GPU offload
  cuda-bridge/         cuda-bridge                  Thin CUDA Driver API bridge for GPU offload
  craton-gpu4j/        cratonvm-gpu                 Build-time-only: packages the @GpuKernel/@Parallel Java annotation sources (from the gpu4j repo) into a jar for jit-cuda's build script; no runtime code
  classloading/        cratonvm-classloading        Class loading & bytecode verification
  gc/                  cratonvm-gc                  Garbage collectors (ZGC is the DEFAULT since 2026-08-10: concurrent-marking, compacting, optionally generational, behind the default-ON `zgc` feature; generational semi-space and G1 remain selectable)
  jfr/                 cratonvm-jfr                 Java Flight Recorder
  vm/                  cratonvm-vm                  VM runtime engine
  vm-cli/              cratonvm-cli                 CLI entry point
  libcratonvm/         libcratonvm                  C-ABI shared library for embedding (cdylib/staticlib libjvm substitute)
  cratonvm-embed/      cratonvm-embed               Semver-stable Rust facade for embedding CratonVM
  difftest/            cratonvm-difftest            HotSpot differential-testing harness
```

The `fuzz/` directory is its own standalone workspace (`cratonvm-fuzz`, nightly-only libFuzzer harness) and is *not* a member of this workspace — its `#![no_main]` `fuzz_target!` expansion trips the production lints, so it builds separately via `cargo +nightly fuzz build`.

**Dependency flow:**
```
vm-cli -> vm -> {classloading, gc, jit, native-builtins, native-collections,
                 native-io, native-awt, jfr}
                 -> {reader, types, native-api, jit-api}
                 -> (gpu-offload feature only) {jit-cuda, cuda-bridge}
                     -> also forwards gc/gpu-offload, native-builtins/gpu-offload

native-builtins -> {native-builtins-crypto, native-builtins-security}

libcratonvm  -> vm   (C-ABI / JNI Invocation API embedding shim)
cratonvm-embed -> vm (curated, semver-stable Rust embedding facade)
```

`craton-gpu4j` does not appear above: it is a *build-dependency* of
`jit-cuda` only (its `build.rs` compiles the GPU annotation sources and
exposes their jar path via Cargo's `links` metadata), never a runtime
dependency of anything.

## reader — Class File Parser

Parses `.class` files per the JVM specification (JVMS Ch. 4).

```
reader/src/
  class_reader.rs       Entry point: bytes -> ClassFile
  buffer.rs             Binary cursor (big-endian reads)
  constant_pool.rs      20 CP entry types
  instruction.rs        200+ bytecode opcodes
  attribute.rs          30+ attribute types
  stack_map.rs          StackMapTable verification frames
  field_type.rs         Field descriptor parsing (e.g. "Ljava/lang/String;")
  method_descriptor.rs  Method descriptor parsing (e.g. "(II)V")
  class_access_flags.rs Bitflag types for access modifiers
  class_file_version.rs Major/minor version constants (Java 1.1-25)
```

The reader is a pure parser with no VM dependencies. It can be used
independently to inspect `.class` files.

## vm — Virtual Machine

The VM is the core of the project (~2,140,000 Rust LoC across the 22 workspace
member crates, plus the separate `fuzz` harness workspace).
It contains six major subsystems (several now extracted into their own
crates).

That figure is the raw line count of every `.rs` file under the 22 directories
named in the root `Cargo.toml` `[workspace] members` list, excluding `target/`,
excluding the non-member `fuzz/` workspace, and excluding vendored third-party
sources under any `vendor/` directory (e.g.
`native-builtins/vendor/rustls-cbc`). Reproduce it with:

```sh
find <the 22 member dirs> -name '*.rs' -type f \
  -not -path '*/target/*' -not -path '*/vendor/*' -print0 \
  | xargs -0 cat | wc -l
```

which reports roughly 2,140,000 lines (2,142,548 on 2026-09-09) across 971
files. Re-measure before quoting it: this figure and the table below stood
at 1,350,000 for long enough to be wrong by 49%, because nothing regenerates
them. If you change this paragraph, change the table too — they are derived
from the same command.

Rough size distribution, largest first, so newcomers know where the mass
actually is:

Measured 2026-09-09 with the command above, one directory at a time;
`native-api` has moved on every jdk-only lane landing since, because every
retirement table lives in `retired_shadow.rs`, one row per retired triple plus
the account justifying it: L1's wave took it from 37k to 38,923; L0+L3+L1's
waves 3-4+L7 took it to 41,715; lane 4 wave 1 and `file_layout.rs` to 42,429;
`dev` reached 43,734 on its own and lane 4 wave 2's 137-row table added 902
more; lane T's table (783 triples after its own carve-out, see the
throwable-family lane record) brings it to **49,223** on the merge of all of
the above. **The 5% tolerance on the row below is roughly two waves wide**, so
expect to re-measure it about every second wave rather than treating a red
here as a surprise.

| Crate | LoC | Crate | LoC |
|-------|----:|-------|----:|
| `native-builtins` | 753,000 | `native-awt` | 18,000 |
| `vm` | 480,000 | `types` | 44,000 |
| `jit` | 269,000 | `native-api` | 49,000 |
| `gc` | 190,000 | `reader` | 17,000 |
| `native-collections` | 88,000 | `jfr` | 20,000 |
| `native-io` | 85,000 | `jit-cuda` | 14,000 |
| `classloading` | 75,000 | remaining 9 | < 14,000 each |

Several individual files are far larger than is comfortable. The two worst have
been split at the section banners they already carried:

- `vm/src/runtime/interpreter.rs` went ~50,500 → ~24,100 lines, with
  `interpreter/typecheck.rs` (`checkcast`/`instanceof`/`aastore` compatibility),
  `interpreter/constants.rs` (`ldc` and loader-faithful `CONSTANT_Class`
  resolution), `interpreter/field_access.rs` (field resolution and invoke
  argument plumbing), and `interpreter/invoke.rs` (method resolution, dispatch,
  and the native bridge — ~23,200 lines, still the single largest thing here
  because method invocation genuinely is one subsystem).
- `jit/src/x64.rs` went ~44,200 → ~36,200 lines, with each optimization pass in
  its own module: `x64/licm.rs`, `x64/licm_int.rs`, `x64/bce.rs`,
  `x64/escape_analysis.rs`, `x64/null_check_elim.rs`, `x64/simd_analysis.rs`,
  `x64/bytecode_compat.rs`, `x64/reg_encoding.rs`, `x64/switch_validation.rs`,
  and `x64/cpu_features.rs`. What remains is the emitter and the compilation
  entry point, which are not a clean seam.

Nothing moved between modules and nothing became more public than it was: each
child is `mod x; pub use x::*;`, and a glob re-export caps every item at its own
declared visibility. Note that `hot_files_have_no_production_panics` enumerates
both directories from disk — a gate that kept scanning only the parent would
have turned this split into "silently stopped checking most of it".

`vm/src/vm.rs` appears to dwarf both at ~74,200 lines, but that number is
misleading and this is **not** the file to start reading. Everything from
line 62 onward is a single `#[cfg(all(test, feature = "synthetic-jdk"))]
mod tests` — a test module the default build does not even compile, because
`synthetic-jdk` is deliberately not a default Cargo feature (`vm/Cargo.toml`).
The orchestrator itself is the ~61-line module header above it, which declares
and re-exports `vm/src/vm/`: `vm_exec.rs` (~21,900 lines), `vm_init.rs`
(~12,300), `vm_util.rs` (~4,600), `vm_object.rs` (~2,100), and `realms/`.
Those are the files to open.

Two `native-builtins` files were worse and have also been split:
`lib.rs` went ~90,000 → ~39,000 across 13 per-domain
modules (`util_concurrent_ext`, `antlr_intrinsics`, `regex_matcher`,
`math_bignum`, …), and `phases_late.rs` went ~77,000 → ~8,000 across 18
modules under `native-builtins/src/phases_late/` (`nio_file`, `bouncycastle`,
`concurrent`, `ssl_security`, `net_channels`, `streams`, `jdbc`, …).
`phases_late.rs` itself now holds only the shared preamble, the per-phase
dispatchers, and cross-domain leftovers.

**Splitting a file does not speed up incremental builds, and it cannot.** Rust's
compilation unit is the *crate*, not the file: moving code into modules of the
same crate leaves that crate — and everything downstream of it — rebuilding in
full. Measured over the 90k→39k split above, `touch lib.rs && cargo build
--release -p cratonvm-cli` went 1m58s/2m00s before to 2m03s/2m03s after, i.e.
noise. The payoff of splitting is **merge-conflict surface and reviewability**
(the two worst files are 57% and 89% smaller, and edits now land in 31 separate
files instead of colliding in two), not build time.

An actual incremental-build win requires **separate crates**. That work has
started at the heaviest stable seams: cryptographic kernels and JDK
security/SunEC code now compile as `native-builtins-crypto` and
`native-builtins-security`. The facade crate retains registration and
Java-object marshalling; other domain modules remain candidates only when a
measured rebuild or ownership benefit justifies another crate boundary.

### Runtime (`vm/src/runtime/`)

The bytecode execution engine.

- **`interpreter.rs`** — Main dispatch loop (~24,100 lines, plus the
  `interpreter/` submodules listed above). Each opcode reads operands,
  manipulates the operand stack and local variables, and advances the program
  counter.

  There are two cooperating dispatch paths:

  * the common *fast path* reads opcodes from verified raw bytecode and fuses
    common sequences (for example `iload_X; iload_Y; iadd`) as
    superinstructions; and
  * unsupported opcodes or guarded edge cases fall through to the shared
    decoded handler.

  Package names are no longer execution-policy inputs: identical verified
  bytecode takes the same common path whether it belongs to an application,
  the JDK, or a framework. `--noverify` disables unchecked raw-byte handlers
  and uses the bounds-checked decoded path. A one-time quickening pass stores
  a `QuickenedCode` (`reader/src/quickened.rs`), whose bitmap/rank index maps a
  bytecode PC to a decoded instruction in O(1), including taken branches.
- **`frame.rs`** — Stack frame: local variables and operand stack use
  8-byte NaN-boxed `CompactValue` slots. A parallel byte array is retained
  for the ambiguous raw `long`/`double` local cases; tags are otherwise inline.
- **`call_stack.rs`** — Per-thread call stack of frames.
- **`value_stack.rs`** — Typed operand stack (SoA encoded).
- **`exceptions.rs`** — Java exception creation and throw handling.
- **`stackwalker.rs`** — `Throwable.fillInStackTrace` / `getStackTrace` and
  `java.lang.StackWalker`. A JIT-compiled method pushes no `Frame`, so this
  splices the active compiled frames back in at the interpreter depth each was
  entered at, and expands the callees an artifact inlined — methods that are
  genuinely executing and that previously contributed no frame at all. As of
  2026-09-01 a warmed-up trace matches HotSpot byte-for-byte on the checked-in
  witness `probes/StackTraceAfterOsr.java`.

  The design point worth knowing is the OSR case. An OSR transfer hands the
  whole rest of a method to compiled code while that activation's interpreter
  `Frame` stays parked at the back-edge it tiered up from, so its `pc` is stale
  for the entire window and only the compiled half knows where control is. The
  bci read out of the compiled half is therefore carried to the surviving frame
  as a **display-only** override that reaches the trace assembler and nothing
  else: it is deliberately never written into `Frame::pc`, because
  `Frame::live_locals_mask_here` computes the per-bci live-locals **GC root
  filter** from `[pc, last_instr_pc]`, so advancing `pc` would stop every slot
  that dies in between being a root — on exactly the frame whose locals the
  conservative half of the JIT root scan is leaning on. (The OSR safe-reject
  exit is also correct only because `frame.pc` is still `entry_pc`.) The
  override is produced only on the arm where the OSR entry site itself named the
  artifact, never on the pc-shape heuristic, and
  `CRATONVM_JIT_NO_OSR_PC_REFRESH=1` reverts the decision and the line together
  inside one binary.
- **`invokedynamic.rs`** — Lambda/method-ref bootstrap via LambdaMetafactory.

### Typed Bootstrap (`vm/src/vm/vm_init.rs`)

Startup is encoded as consuming typestate transitions:

```text
BootstrapPhase<Allocated>
    → BootstrapPhase<ClassesReady>
    → BootstrapPhase<NativesReady>
    → BootstrapPhase<RuntimeReady>
```

Each boundary validates the invariant it owns: a nonempty class universe with
`java/lang/Object`, a nonempty native registry, and wired runtime hooks.
Only `RuntimeReady` exposes `finish`. The token is deliberately small; large
subsystems remain locally owned during `SharedVm::new`, while the token prevents
accidental reordering and gives tests a precise failure boundary.

The end-to-end process lifecycle and the cross-subsystem safety invariants are
documented in the manual's
[Runtime Lifecycle](docs/book/src/internals/runtime-lifecycle.md) and
[Runtime Contracts](docs/book/src/internals/runtime-contracts.md) chapters.

### Class Loading (`classloading/` crate)

Implements JVMS Ch. 5: loading, linking, and initialization. Extracted into the
`cratonvm-classloading` crate.

- **`class_manager.rs`** — Central class cache and loading coordinator.
- **`loaders.rs`** — Bootstrap, extension, and application class loaders.
- **`class_path.rs`** — Classpath scanning (directories + JARs via `zip` crate).
- **`class.rs`** — Runtime class representation with metadata and hierarchy.
- **`verifier.rs`** — Structural verification (JVMS 4.8-4.9).
- **`bytecode_verifier.rs`** — Type-checking verification (JVMS 4.10).
- **`vtype.rs`** — Verification type lattice.
- **`resolution.rs`** — Symbolic reference resolution with inline caching.
- **`access_control.rs`** — Access checking (JVMS 5.4.4).

### Memory (`gc/` crate)

Garbage collectors, extracted into the `cratonvm-gc` crate. **ZGC is the default collector**, and has been since 2026-08-10: `gc/Cargo.toml` carries `default = ["zgc"]` and `vm/src/config.rs` defaults `gc_algorithm` to `GcAlgorithm::Zgc`, so a stock `cargo build` both contains ZGC and runs it. `ZgcRealHeap` (`gc/src/zgc.rs`, with `gc/src/zgc_concurrent.rs` and the twelve modules of `gc/src/zgc/`) is a real, memory-backed collector — `Arena` storage, real `ObjectHeader`s, real `java.lang.ref` processing, a default-ON TLAB serving `alloc_object` / `alloc_array` — wired end to end as `GcAlgorithm::Zgc` → `GcBackend::Zgc` → `VmHeap::Zgc`, so `-XX:+UseZGC` also names it explicitly. It has colored pointers (`zgc/vaddr.rs`), a load barrier (`zgc/barrier.rs::z_load`), concurrent marking (`zgc_concurrent.rs`, `CRATONVM_ZGC_CONC_START`), compaction (`zgc/relocate.rs`, kill switch `CRATONVM_ZGC_RELOCATE=0`) and an opt-in generational mode (`zgc/generation.rs`, `CRATONVM_ZGC_GENERATIONAL=1`). The generational collector — young + old gen, with the Cheney moving young gen *requested* by default on that backend, but see **"When the generational backend does not compact"** below — stays selectable with `-XX:+UseGenerationalGC` and is what a `--no-default-features` build falls back to. A region-based G1 collector is opt-in via `-XX:+UseG1GC` (experimental — see `docs/feature-designs/concurrent-gc-maturation.md`). Above `ZgcRealHeap` in the same file sits a metadata-only *simulation* of OpenJDK's colored-pointer model (`ZgcCollector` / `ColoredPointer` / `LoadBarrier` / `GenerationalZgc`) with no production consumer; do not read those type names as the shipping path.

> **The colored-pointer load barrier is plumbed but not armed, and that distinction is the entire status.** *Plumbed:* `gc/src/vm_heap.rs::load_ref_slot_barriered` is the backend-dispatching seam, delegating to `ZgcRealHeap::load_barrier_slot` on the `Zgc` arm and refusing under compressed oops, because a colored word does not fit in a 4-byte slot (ZGC and compressed oops are mutually refused at VM init in any case). `types/src/narrow_oop.rs`'s `read_ref_slot` / `write_ref_slot` and ZGC's own compaction slot-rewrite are now `Relaxed` atomics rather than plain accesses, because a plain write racing the barrier's self-healing `compare_exchange` is undefined behaviour and not merely a lost update. The JIT's compact-field reference read routes through the seam, and the inline `aastore` arm (`jit/src/x64/bytecode_walk.rs`, opcode `0x53`) now consults the same armed gate its compact-field siblings already did, falling back to the fully chokepointed `jit_aastore` helper. *Not armed:* `vm/src/vm/vm_init.rs` pins `const RELOCATION_REQUESTED: bool = false`, `ZgcRealHeap::set_barrier_color` — the sole writer of the colored state — has no non-test caller, and `barrier_good_mask` never leaves `Z_REMAPPED`, so every shipping configuration evaluates the same plain read it did before and all of the above is inert by construction. Still open, in order: `jit_aaload` receives no `vm_ptr` and so cannot reach a `&VmHeap` without an ABI change, and it is the hotter of the two slot-holding sites; the sites that hold an `ObjectRef` rather than a slot (`types/src/value.rs::read_value_atomic`'s reference arm, `vm::get_static_shared`) are blocked on static and legacy slots not being atomic; and arming must happen at a safepoint, because an emission-time gate cannot reach already-compiled code. The ordered work list with each step's status is the doc comment on `VmHeap::load_ref_slot_barriered`; the counters that would show the barrier had actually run are `vm/src/jit/helpers.rs::ref_load_census` (`BARRIERED_LOADS`, `UNBARRIERED_LOADS`, `COLORED_WORDS_SEEN`). Design: `docs/feature-designs/zgc-jit-load-barrier.md`; the rest of the plan: `docs/feature-designs/zgc-production-implementation-plan.md`.

- **`heap.rs`** — Object/array layout and allocation (semi-space).
- **`gen_heap.rs`** — Generational heap: young gen (copying) + old gen.
- **`gc.rs`** — Cheney copying collector algorithm.
- **`collector.rs`** — GC coordination and stop-the-world orchestration.
- **`card_table.rs`** — Card marking for cross-generational references.
- **`arena.rs`** — Bump-pointer memory arena.
- **`roots.rs`** — Root scanning and pointer remapping.
- **`old_gen.rs`** — Old generation management.

**When the generational backend does not compact.** The flag gate is gone; the
per-cycle proof is not. Read both before reasoning about allocation-path or GC-pause code.

1. `moving_young` is an **opt-out** flag that defaults to **true** —
   `types/src/flags.rs::DEFAULT_MOVING_YOUNG`, shaped as
   "`CRATONVM_NO_MOVING_YOUNG` turns it off, `CRATONVM_MOVING_YOUNG` is a
   retained no-op opt-in", and pinned by
   `empty_source_matches_all_documented_defaults` and
   `moving_young_is_an_opt_out_with_a_compatibility_opt_in`. A default
   `cargo build` therefore *requests* a moving young cycle.
2. Requesting one is not running one: every cycle must carry a **per-cycle
   coverage proof** before it may relocate. `collect_garbage_inner` diverts to
   the non-moving sweep on `divert_for_incomplete_moving_coverage`, which
   `vm/src/memory/roots.rs` computes from
   `conservative_roots::refresh_moving_young_coverage_for_collection()` — every
   live compiled frame's active safepoint must certify
   `moving_young_coverage_complete`, no unregistered JIT frame may be on the
   native stack, no peer thread may be in JIT, and a frame-band verifier must
   find no young-resident word the shadow stack did not publish. Each diversion
   is counted and logged at `warn` with the specific unproven obligation
   (`gc_quiescence::incomplete_reason`), and `--verbose:gc` / `CRATONVM_GC_STATS`
   print `moving_young: cycles=N coverage_fallbacks=M` plus a per-reason
   histogram.
3. **Ask the runtime, not a document.** `gc_metrics::collector_decision_report()`
   (`gc/src/gc_metrics.rs`) renders the last collection's actual decision —
   backend, moving/non-moving verdict, the stable reason code, and the specific
   unproven obligation — and `print_gc_summary` emits it on every `--verbose:gc`
   run. It is the authority for "did this process compact?"; prose about the
   flag is not. `docs/GC.md`'s backend table is kept in step with that report; when the two
   disagree, the runtime report wins.

   (An earlier `fail_closed_non_moving = is_active() && !allow_moving_young`
   term made this unconditional — it meant `CRATONVM_MOVING_YOUNG=1` alone could
   never run a moving cycle under a live JIT frame, the only case the feature
   exists for. That term and the `CRATONVM_ALLOW_MOVING_YOUNG` flag are gone.)

So on a JIT-warm workload a run on the generational backend can still spend
cycles in the **generational non-moving mark-sweep with selective promotion** path — that is
what a nonzero `coverage_fallbacks` count means, and it is the number to read
before attributing a pause profile to compaction.

Correctness is no longer the blocker: the heap corruption was
five codegen sites pushing an untagged object reference onto the JIT's
simulated operand stack. Throughput is the **remaining** work, now tracked as
an optimization program rather than as a precondition, because the memory-
footprint win decided the flip: on Binary-Trees-18 the moving path measures
**2.1×** the sweep (six interleaved rounds, min 2905 ms vs 1363 ms), down from
5.0× before the pre-cycle object-start walk stopped building a hash set of
every object in from-space — but bt18 at `-Xmx512m` does not complete at all
without compaction. bt18 is simultaneously the worst case for a copying
collector and the workload that justifies it. The open items are the `pointer_map`
`FxHashMap`, the disabled self-call spill elision, and the unpriced
`jit_frame_record` helper.

**Object layout:**
```
[ObjectHeader (16 bytes)] [field0] [field1] ...
```

Field cell width depends on the field's type and on the layout in force
(`types/src/heap_types.rs`, `types/src/field_layout.rs`):

| Field kind | Width | Notes |
|------------|------:|-------|
| Reference | 8 B | Bare pointer, `0` = null; 4 B when compressed oops are explicitly enabled. |
| `boolean` / `byte` | 1 B | Tagless physical field. |
| `char` / `short` | 2 B | Tagless physical field. |
| `int` / `float` | 4 B | Tagless physical field. |
| `long` / `double` | 8 B | Tagless physical field. |

`CompactLayout` computes aligned offsets and the precise GC reference map
(`ref_offsets`). The interpreter/native `Value` enum is a boundary type, not
the physical instance-field representation.

The header is **16 bytes** (`HEADER_SIZE`, `types/src/heap_types.rs`):
`class_id` (4) + `shape` (4) + `mark_word` (8).

This paragraph used to describe a 32-byte header and call compression "an
experiment, not an unfinished requirement", listing the two reasons it could not
shrink: folding forwarding state into the mark word would couple relocation to
monitor state, and removing the identity hash alone saves nothing after
alignment. Both were answered rather than avoided. The mark word absorbed the
header three times over 2026-08-06/07 -- `forwarding_ptr` (32 -> 24), then
`identity_hash_code`, then the `kind` / `element_type` / `gc_age` / `gc_flags`
quartet into bits 48..63 (24 -> 16). The identity-hash fold is the instructive
one: it did buy zero on its own, exactly as the old paragraph said, and it was
the prerequisite for the eight bytes the quartet's move then paid out.

Arrays use compact element sizes (1/2/4/8 bytes per element depending on type;
`element_byte_size`), with reference elements at `REF_ELEMENT_SIZE` = 8 B.

**Compressed oops** (`gc/src/compressed_oops.rs`) *are* wired into the live
heap now — `enable_for_live_heap` is called from `vm/src/vm/vm_init.rs:874` —
but behind an **opt-in** `-XX:+UseCompressedOops` / `CRATONVM_COMPRESSED_OOPS=1`
gate, and `use_compressed_oops` defaults to `false` (`vm/src/config.rs:426`), so
the default path does not use it. When the gate is on, reference instance fields
and reference array elements narrow from 8 bytes to 4; the klass pointer is
deliberately not narrowed (`ObjectHeader::class_id` is already a `u32`). The
reason the gate stays off is a throughput regression, not incompleteness: the
JIT's inline compact-field fast paths are disabled while compressed oops are
active, so `getfield`/`putfield` fall back to the always-correct helpers.

### JIT Compiler (`jit/` crate)

Custom JIT compiler: a single-pass x86-64 emitter, a sea-of-nodes IR tier that
also lowers to x86-64, and an AArch64 backend. It lives in the `cratonvm-jit`
crate (about 268,000 lines of Rust under `jit/src/`, including `jit/src/x64/`).
Shared API types, including the runtime helper table `JitRuntimeHelpers`, are in
`cratonvm-jit-api`; see [docs/jit/helper-abi.md](docs/jit/helper-abi.md).

**Layout.**

| Area | Files | Role |
|---|---|---|
| Crate root | `jit/src/lib.rs` | `CompiledMethod`, `JitCache`, `try_compile` / `try_compile_with_invokespecial_resolver` / `try_compile_inner` (the IR orchestration and its fall-through), the inlining planner, the escape-analysis bridge, OSR entry (`osr_enter`, `osr_trampoline`, `ir_osr_enter`) |
| Admission | `jit/src/compile_gate.rs` | `compile_gate::admit`, the door every backend entry passes (`CompileDoor::{MethodEntry, EagerFirstCall, Osr}`); returns the `CompileAdmission` the backends require |
| Single-pass x64 emitter | `jit/src/x64.rs` (module root) and `jit/src/x64/*` | `x64/driver.rs` `compile_with_param_slots` (production entry; `x64::compile` is the legacy test/AOT wrapper) → `x64/bytecode_walk.rs` `Compiler::compile_bytecode`, the per-opcode walk. Frames in `frames.rs`, safepoints/shadow stack/oop maps in `safepoint.rs`, deopt stubs in `deopt_stubs.rs`, OSR exit maps in `osr.rs`, plus inlining, LICM, BCE, instruction patterns, SIMD and a bytecode-level escape analysis |
| IR tier | `ir.rs` → `ir_optimize.rs` → `ir_verify.rs` → `ir_schedule.rs` → `ir_lower.rs`, accepted through `ir_evidence.rs` | `IrBuilder` and `ir_compatible`; optimization passes; the always-on verifier; block placement; lowering to x86-64, which runs the `regalloc.rs` linear-scan allocator itself. Supporting analyses: `escape_analysis.rs`, `ir_check_elim.rs`, `range_analysis.rs`, `scev.rs`, `loop_analysis.rs` |
| Shared lowering | `jit/src/runtime_lowering.rs` | allocation, monitor (`emit_monitor_stub`) and hashed-vtable stubs used by both x64 front ends |
| Register allocation | `jit/src/regalloc.rs` | Chaitin-Briggs colouring of bytecode locals for the single-pass emitter; `allocate_linear_scan` / `verify_allocation` for the IR lowerer |
| AArch64 | `aarch64.rs`, `aarch64_backend.rs` | see [docs/jit/aarch64-parity.md](docs/jit/aarch64-parity.md) |
| Deopt / OSR | `deopt.rs`, `osr_contract.rs`, `osr_coords.rs`, `osr_exit.rs`, `x64/osr.rs`, `x64/deopt_stubs.rs` | frame states and deopt points; OSR entry and exit contracts |
| Tiering and profiles | `tiered.rs`, `profile.rs`, `pgo.rs` | `TieredCompilationManager`, compilation queue and background compiler; interpreter-collected profiles |
| VM glue | `vm/src/jit/*`, `vm/src/runtime/interpreter/jit_bridge.rs`, `vm/src/runtime/interpreter/deopt_resume.rs` | runtime helpers and `build_helpers` (`vm/src/jit/helpers.rs`), JIT-frame GC roots, code-cache lifecycle; every place the interpreter asks for, enters or leaves compiled code; rebuilding interpreter frames after a deopt |

**Flow: interpreter → tier-up → compile → install → deopt / OSR.**

1. **Interpret and count.** Dispatch sites report invocations to
   `TieredCompilationManager::on_method_invocation_observed`. A taken back edge
   reaches `try_osr_with_backoff` (`vm/src/runtime/interpreter.rs`), which asks
   `TieredCompilationManager::request_osr`. The first-tier threshold is
   `CRATONVM_JIT_THRESHOLD` (default 500; see `vm/src/runtime/env_cache.rs`).
   Tier policy lives in `jit/src/tiered.rs`.
2. **Tier up.** The manager yields a `CompilationTask`. Codegen runs
   **off-thread by default**: `jit_bridge::background_compile_task` compiles and
   publishes while the mutator keeps interpreting (`CRATONVM_BG_COMPILE=0`
   restores inline compilation). The interpreter also has an eager first-call
   single-pass door.
3. **Compile.** Every door passes `compile_gate::admit`.
   * The **single-pass** path is `x64::compile_with_param_slots` →
     `Compiler::compile_bytecode`.
   * The **IR** path runs inside `try_compile_inner` when the caller asked for
     the optimizing backend and `ir::ir_compatible` holds. The background worker
     asks for it when `tiered::tier_uses_optimized_backend` says so, which is the
     `C2` and `FullProfile` tiers. The steps are: `IrBuilder` → the
     `IR_MAX_GRAPH_NODES` (20,000) cap → `ir_optimize::optimize` → `ir_verify` →
     `ir_schedule::schedule_with_options` → `ir_lower::lower_inner` →
     `ir_evidence::accept`. `accept` publishes the IR body only when the
     recorded transforms justify replacing the baseline
     (`CRATONVM_C2_ACCEPT=always|evidence|never`, default `evidence`).
   * Any IR refusal, at any step, falls through to the single-pass path.
4. **Install.** The artifact goes into the VM's `JitCache` (`JitCache::put`, or
   `JitCache::put_osr` for OSR artifacts). Lifecycle:
   [docs/jit/code-cache-lifecycle.md](docs/jit/code-cache-lifecycle.md).
5. **Enter.** `jit_bridge::execute_jit_call` (and its decoded / one-shot
   variants) calls the compiled body.
6. **Deopt.** A failing guard or uncommon trap calls `jit_uncommon_trap`
   (`vm/src/jit/helpers.rs`). That stashes a reconstructed frame, which the sink
   takes with `deopt::take_last_deopt`, and returns `i64::MIN`. The sink then
   either resumes precisely in the interpreter (`deopt_resume.rs`) or re-runs
   the whole method.
7. **OSR.** `try_osr` compiles or reuses an OSR artifact
   (`compile_osr_artifact`, or `compile_optimizing_artifact` for the IR door),
   admits the entry (`CompiledMethod::validate_osr_entry` for single-pass
   artifacts), and enters through `osr_enter_planned` or `ir_osr_enter`. A mid-loop
   exit is transferred into the live frame by
   `transfer_osr_exit_into_live_frame_checked`. See
   [docs/jit/on-stack-replacement.md](docs/jit/on-stack-replacement.md).

**The two front ends now share one runtime-sensitive lowering library.**
`jit/src/runtime_lowering.rs` owns the x86-64 contracts for allocation,
megamorphic dispatch, and live monitor calls. The single-pass emitter and IR
lowerer may differ in optimization and scheduling, but both emit those
stateful operations through the same ABI.

`ir_compatible()` admits up to 64 invokes (`IR_MAX_INVOKES`), 64
instance-field operations, 64 static-field operations, 64 `new` sites
(`IR_MAX_ALLOCATIONS`) and 16 array-allocation sites
(`IR_MAX_ARRAY_ALLOCATIONS`), in methods of at most 8,000 bytes
(`IR_MAX_BYTECODE_SIZE`, the only cap `ir_compatible_sized` applies). The
20,000-node cap (`IR_MAX_GRAPH_NODES`) is checked in `try_compile_inner` after
the graph is built. Static calls lower directly, virtual/interface calls use the
same MIC/four-entry PIC plus compact eight-set/two-way hashed tail, and escaping
`Op::New` nodes lower through the class-initializing, TLAB-aware allocation
helper. Live monitor bytecodes lower through `runtime_lowering::emit_monitor_stub`
to the `monitor_enter` / `monitor_exit` helpers. The IR tier elides the monitors
of an object escape analysis proves confined, all-or-nothing per object, and a
method that had monitors elided cannot deopt-resume precisely
([docs/jit/lock-elimination.md](docs/jit/lock-elimination.md)).

Coverage gaps remain fail-closed admission boundaries: the method uses another
tier or the interpreter rather than receiving different runtime semantics.
`invokedynamic` is no longer one of them for the method as a whole. Both
backends lower an `invokedynamic` site to an uncommon trap, and an OSR entry
into such an artifact is refused (`osr-entry-unconditional-trap`).
`multianewarray` compiles in the single-pass emitter only for two dimensions
(the `multianewarray_2d` helper); the IR tier refuses the opcode, and such a
method falls back to the single-pass path.

Key optimizations: register allocation for locals, magic division,
LICM, bounds check elimination, AVX2 SIMD, on-stack replacement (OSR).
Inlining is tiered: trivial callees up to 6 bytes, cold callees up to 35 bytes,
and profile-hot callees up to 325 bytes, with total budgets of 750 bytes (cold)
or 2,000 bytes (hot). There is still no general cross-call register allocation
and no bounded-depth recursion inlining; see [BENCHMARK.md](BENCHMARK.md) for
what that costs on recursion-bound rows.

Two calling conventions (`CompiledMethod::needs_context`): **pure** methods
(direct call) and **context** methods, which receive the `SharedVm` pointer as a
hidden first argument. Whether a method needs the context is an output of
optimization: escape analysis can remove its last heap use.

### Native Methods (`native-api/` and `native-*` crates)

A large compatibility surface of native implementations and application
bridges, split across domain-specific crates. The real-JDK default registers a
smaller bridge/intrinsic subset; the `synthetic-jdk` feature adds the synthetic
standard-library surface. `native-api/` defines narrow heap, class, invoke,
thread, exception, system, and related capability traits composed by
`NativeContext`.

- **`native-builtins/`** — java.lang.*, registration, and Java-object
  marshalling.
- **`native-builtins-crypto/`** — separately compiled cryptographic
  compatibility kernels.
- **`native-builtins-security/`** — separately compiled JDK security and SunEC
  implementation pack.
- **`native-collections/`** — java.util.* native methods.
- **`native-io/`** — java.io/nio native methods.

These stubs are the **synthetic** standard library — the fallback mode, not the
default one (see Key Design Decision 1). In the default real-JDK mode the same
crates register a much smaller essential-native surface underneath real
`java.base` bytecode, so a method you find implemented here may not be the one
executing. The `NativeContext` capability facade provides VM-agnostic,
loader-aware operations in both modes without making native crates depend on
the concrete VM. Common native argument decoding, forwarding, and pin-index
buffers retain up to eight slots inline and spill correctly for larger
descriptors.

### Threading (`vm/src/threading/`)

- **`jvm_thread.rs`** — Per-thread state (call stack, printed output).
- **`thread_registry.rs`** — Global thread tracking.
- **`monitor.rs`** — Object monitors (synchronized/wait/notify).
- **`gc_barrier.rs`** — Stop-the-world safepoint coordination.
- **`virtual_scheduler.rs`** — Virtual thread scheduler (Java 21+).

## GPU Offload (opt-in, `--features gpu-offload`)

Entirely feature-gated: a default build links none of this and the
interpreter's hot path carries zero extra branches. Gating features:
`gpu`/`gpu-driver` on `cratonvm-cli`, `gpu-offload` on `cratonvm-vm` (which
forwards to `cratonvm-gc` and `cratonvm-native-builtins`). See
[BUILD_GUIDE.md](BUILD_GUIDE.md#building-with-gpu-offload) for the build
levels and [docs/gpu/README.md](docs/gpu/README.md) for the full reference.

**Crates.** `jit-cuda` lowers Java bytecode to PTX: `analyzer.rs` decides
whether a static method is GPU-eligible (primitives only, no allocation,
calls, fields, or reference arrays), `lowering.rs` / `loop_recog.rs` /
`emit.rs` turn an eligible method's counted loop into PTX text. `cuda-bridge`
is a thin CUDA Driver API wrapper with a no-driver `backend_stub.rs`
default and a real `backend_cuda.rs` (behind `cuda-bridge/cuda`) built on
`cudarc`.

**Pipeline.** The interpreter's `execute_invokestatic` hook
(`vm/src/runtime/interpreter.rs`) calls `runtime::offload::try_dispatch`
(`vm/src/runtime/offload.rs`), which asks the per-VM `OffloadCache` to
analyze-and-lower a callee once and cache the resulting `CompiledKernel`
(a loaded PTX module) by `(ClassId, method_index)`. On a cache hit,
`dispatch_method_from_native` / `dispatch_async` marshal the Java
primitive-array arguments — via `vm/src/runtime/gpu_marshal.rs`'s
`host_view_<T>`/`write_back_<T>` packed-copy path, or a zero-copy DMA
straight against the JVM heap arena once an array's element type is
proven — then hand them to `cuda-bridge`'s `DeviceContext`. The context
runs the upload, launch, and download on three CUDA streams (`copy_h2d`,
`compute`, `copy_d2h`) ordered by CUDA events rather than a blocking sync
per stage. Kernels signal failure (e.g. an out-of-bounds index) by writing
a device `failure_flag` word instead of throwing; `finalize_submission`
checks it once the event chain completes and deopts to the CPU
interpreter — leaving no partial GPU state in the heap — on a failure,
or copies results back and resumes the Java frame on success.
`vm/src/runtime/gpu_residency.rs` separately tracks longer-lived
`GpuArray<T>` host/device residency for the explicit async API, independent
of this transparent per-call path.

**GC coordination.** While kernel arguments are in flight, the calling
thread holds a `SafepointToken` from `Heap::enter_gpu_critical()` and pins
the argument `ObjectRef`s via `Heap::pin_ref` (both in `gc/src/heap.rs`).
The collector checks the resulting `gpu_critical_count` and yield-spins
rather than moving or reclaiming while any token is alive; the root walker
visits pinned refs so a kernel never reads through a stale or relocated
pointer.

## vm-cli — Command-Line Interface

`clap`-based entry point (~6,000 LoC; `main.rs` is ~4,900). Parses arguments and
`-XX:` flags, constructs a `Vm`, calls `main(String[])`, and handles exit codes.

## Key Design Decisions

1. **Two standard libraries, and the default one is the real JDK's.** This
   decision has been reversed since it was first written, and the old wording
   ("no `JAVA_HOME`, no `rt.jar`, no dependency on any JDK installation at
   runtime") no longer describes the default build. CratonVM carries **two
   complete implementations of the Java standard library**:

   * **Real-JDK mode (the deterministic launcher default).** Class bytecode is
     loaded from an explicitly validated JDK —
     `$JAVA_HOME/jmods/java.base.jmod` or the `lib/modules` jimage, via the
     jimage reader — and Rust supplies only the essential native surface
     underneath it. `VmConfig::for_launcher()` always chooses this mode;
     detection validates availability but no longer selects the library.
   * **Synthetic mode.** A large Rust stub library stands in for `java.*`, so
     the VM can run with no JDK on the machine. It must be selected explicitly
     with `--synthetic-jdk`, and the launcher rejects that selection when the
     binary was not built with the `synthetic-jdk` Cargo feature.

   `--real-jdk` and `--synthetic-jdk` are symmetric and mutually exclusive.
   `VmConfig::default()` remains synthetic for hermetic embedding/tests, while
   the launcher default is the named `LAUNCHER_DEFAULT_JDK_MODE`. Version and
   fatal-error output identify the selected mode. `VmConfig::with_host_jdk_default()`
   is kept only as a compatibility alias for `VmConfig::for_launcher()` — see
   the doc comment on either in [`vm/src/config.rs`](vm/src/config.rs).

   Practical consequence for anyone diagnosing a failure: **establish which
   library the run used before anything else.** A missing real-JDK native and a
   synthetic-stub gap produce indistinguishable stack traces, and a fix applied
   to the wrong one changes nothing on the path that actually failed.

2. **Two-layer exception model.** `MethodCallFailed::ExceptionThrown` wraps
   Java-catchable exceptions; `MethodCallFailed::InternalError` wraps VM
   bugs. The interpreter catches the former at catch/finally blocks.

3. **Compact frame values.** Frames and operand stacks normally encode tag and
   payload together in an 8-byte `CompactValue`. Locals use a parallel
   one-byte kind only to disambiguate raw `long`/`double` patterns and preserve
   JVM category-2 slot semantics.

4. **Direct bytecode-to-x86-64 first, a sea-of-nodes IR for the hot tier.**
   Every door into compiled code can take the single-pass emitter
   (`x64::compile_with_param_slots`), which compiles bytecode directly to
   machine code. That keeps the path simple, at the cost of limiting
   cross-instruction optimization. The eager first-call compile is always
   single-pass. The sea-of-nodes IR is the backend of the optimizing tiers:
   compiles for the `C2` and `FullProfile` tiers
   (`tiered::tier_uses_optimized_backend`) and optimizing OSR entries try it
   when `ir::ir_compatible` admits the method. Its body replaces the baseline
   only if `ir_evidence::accept` finds the recorded transforms worth it; any
   refusal falls back to the single-pass body.

5. **Dense, memoized native dispatch.** Name resolution hashes the
   `(class, method, descriptor)` triple and verifies the full strings on every
   digest hit. The result is a stable dense `NativeMethodId`; a
   `NativeCallSite` caches the registry generation plus slot, so a warm call is
   an atomic load, generation comparison, and bounds-checked array access.
   Call sites that still use `NativeMethodRegistry::find` pay the full string
   hash and should migrate to the shared memo mechanism.

6. **Threading: one OS thread per Java thread.** `thread_start`
   (`vm/src/vm/vm_exec.rs`) spawns a real `std::thread::Builder` per
   `Thread.start()`; virtual threads are multiplexed over carriers by
   `virtual_scheduler.rs`. Beware stale comments elsewhere in the tree that
   describe Java execution as single-OS-threaded under cooperative scheduling —
   that has not been true since real thread spawning landed, and anything
   resting on it (notably the `unsafe impl Send/Sync for ObjectRef` argument in
   `types/src/value.rs`) should be read with that in mind.

7. **Configuration is env-var-driven, and the surface is large.** There are
   hundreds of distinct `CRATONVM_*` identifiers across the workspace. There are
   roughly 530 direct `std::env::var` / `var_os` source call
   sites in the core directories; `vm/src/runtime/env_cache.rs`
   contains 40 itself. The typed and grouped flag layers centralise many
   identifiers, but direct reads remain scattered. Most are
   debug or diagnostic gates, but a meaningful subset changes semantics
   (`CRATONVM_COMPACT_REF_FIELDS`, `CRATONVM_REAL_NET_SOCKETS`,
   `CRATONVM_REAL_FORKJOINPOOL`, `CRATONVM_JIT_GETFIELD_HELPER`,
   `CRATONVM_BG_COMPILE`, …). Most are cached in a `OnceLock` on first read, but
   not all — check before adding one to a hot path, and prefer extending
   `VmConfig` / `runtime::env_cache` over introducing a new bare
   `std::env::var` call.

8. **Compatibility policy is a runtime token in `types`, not a build feature.**
   `--jdk-only` (`CompatibilityMode::JdkOnly`) says *real class bytes are
   authoritative*; the default `CompatibilityMode::Compatible` is today's
   behaviour, byte for byte. It is **orthogonal** to `JdkMode` — that picks
   *which class library* boots, this picks *which substitutions are permitted* —
   and `JdkMode` was deliberately not overloaded with strictness. The default is
   `Compatible` on **both** entry points (`LAUNCHER_DEFAULT_COMPATIBILITY_MODE`
   and `EMBEDDED_DEFAULT_COMPATIBILITY_MODE`, `vm/src/config.rs`), and you reach
   it by doing nothing. Contract:
   [`docs/feature-designs/jdk-only-mode.md`](docs/feature-designs/jdk-only-mode.md).

   Four structural shapes were **forced** here, and each is worth understanding
   before touching this code, because each has an obvious alternative that does
   not work:

   * **The policy token lives in `types`, alone.** The policy has to be legible
     to both the native registry (`native-api`) and the class loader
     (`classloading`), and those two are *peers* in the dependency flow above —
     neither can see the other. The only crate below both is `types`, so
     `types/src/compat.rs` holds `CompatibilityMode` / `ExecutionPolicy` and
     nothing else about the feature. `ExecutionPolicy` carries a bare
     `real_jdk: bool` rather than a `JdkMode` for the same reason: `JdkMode`
     lives in `vm`, which `types` cannot see.
   * **The predicates live next to their own types, not in one place.**
     `NativeKind::allowed_in(mode)` stays in `native-api` and
     `ClassOrigin::allowed_in(mode)` stays in `classloading`. A single
     `fn allowed(kind, origin, mode)` would need both enums in one scope, which
     means either a new edge between two crates that must not depend on each
     other, or dragging both enums (and their whole surfaces) down into `types`.
     The shared thing is only the mode; the judgements stay local to the type
     that can answer them.
   * **One policy-aware dispatch resolver, which every path routes through.**
     `resolve_dispatch` / `DispatchDecision` (`vm/src/vm/vm_exec.rs`) is the
     single native-vs-bytecode decision point for the interpreter, JIT,
     reflection, JNI and method handles. This is not tidiness: the repository
     already had two independent override gates that **disagreed** about
     `java/lang/String` and carried comments asking that they be kept in sync by
     hand. A policy evaluated at N call sites is N policies, and the divergence
     is invisible until something silently takes the wrong branch. The resolver
     exists and the main interpreter path goes through it; paths that
     still bypass it are marked `// JDK-ONLY-WAVE2:` so they can be found
     mechanically rather than by memory.
   * **Policy state is per-VM, never a process global.** It lives on `VmConfig`
     and is pushed into the native registry and the `ClassManager` at VM init,
     before any registration pass. A process global would break
     multi-VM-in-one-process runs, and this repository has a documented history
     of exactly that failure with process-global native caches leaking across
     VMs. Two pre-existing globals reachable from this feature's paths are
     logged as violations to remove, not as precedent.

   Two consequences that surprise people. First, `Class` now carries a
   `ClassOrigin` (boot image, application classpath, user-defined, array, hidden,
   lambda, proxy, reflection accessor, VM-internal, compatibility stub), and the
   older `is_synthetic_stub` bool is a **derived mirror** of it — write both
   through `Class::set_origin`, never one alone. The bool survives because ~160
   read sites across 17 files still use it; collapsing it is a later wave.
   Second, this capability deliberately violates the "ship opt-out" convention
   below, and is the one shape that legitimately can: an opt-out flag makes a
   capability run by default, and a policy whose whole job is to *refuse* work
   cannot be default-on without changing what every existing program does. So it
   is recorded here as what it is — an internal diagnostic, **off by default**
   (`CompatibilityMode::Compatible`, `vm/src/config.rs`), enforcing at class
   fabrication and synthetic-native registration only, with the remaining
   dispatch paths counted rather than blocked. See
   [ROADMAP.md](ROADMAP.md#jdk-only-mode---jdk-only) for the staged rollout that
   is meant to end that exception.

## How to tell whether a feature actually runs

This repository has a chronic and well-evidenced failure mode: a capability
lands behind a `CRATONVM_*` environment variable, the variable defaults to
**off**, the work is recorded as "implemented", and the code never executes on
the default path. The docs then describe a VM that nobody is running. Several
of the corrections recorded in this document were instances of this.

**Confirmed instances** (all verifiable in the tree today):

| Capability | Where the default lives | Status |
|------------|-------------------------|--------|
| `moving_young` | `types/src/flags.rs::DEFAULT_MOVING_YOUNG` | **Resolved.** Restructured as opt-out with a compatibility opt-in, and the compiled default is now `true`. The former independent `allow_moving_young` gate has been deleted. Retained here as the worked example of the fix shape, not as an open instance — but note the second gate: a moving cycle still needs its per-cycle coverage proof, so "flag on" and "compaction ran" remain distinct claims. |
| `use_compressed_oops` | `vm/src/config.rs` | Opt-in, defaults false. Fully wired but never enabled on the default path — and the doc claimed for a while that it was *not* wired, which is the same failure mode in the opposite direction. |
| `safepoint_reg_spill` | `jit/src/x64.rs:2650` | **Was** the canonical case: several call sites' own comments claimed a register spill as their protection, but that spill "only ran when the SEPARATE `CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var was ALSO set — off by default, so the documented protection never actually happened." Now folded into default-on `precise_maps` and inverted to opt-**out** (`CRATONVM_NO_PRECISE_REG_SPILL`). This is the shape the fix should take. |
| `precise_maps` register-spill half | `jit/src/x64.rs:2646-2662` | Same item; `precise_maps` was default-on while its register-spill branch was not. A flag being on does not mean all of its branches are. |

**Checklist — apply this before recording anything as "implemented":**

1. **Find the default.** Look the flag up in `types/src/flags.rs` (or
   `vm/src/runtime/env_cache.rs` / `vm/src/config.rs`). `present(src, "…")`
   means *presence is truth* — the flag is **off** unless the variable exists.
   Do not infer the default from the call site.
2. **Check that the default path takes the new branch.** Read the condition
   that guards it with the flag at its default value substituted in. If the
   branch is unreachable that way, the capability does not ship.
3. **Check for a second gate.** `moving_young` formerly needed
   `allow_moving_young` too, and `precise_maps` formerly needed
   `safepoint_reg_spill`; both duplications have since been removed. They
   remain examples of why one flag being on is not evidence that a code path
   runs.
4. **Check whether the old path was deleted or merely bypassed.** If the
   previous implementation is still present and still reachable, the new one is
   an alternative, not a replacement — and the old one is what most runs use.
5. **State the default in the sentence.** Not "X is implemented" but "X is
   implemented and is **on/off** by default (`FLAG_NAME`, `path/to/file.rs:NNN`)".
   A doc sentence that does not name the default is not a claim that can be
   checked.

**Convention for new work:** a new capability should ship **opt-out**, never
opt-in. Land it on by default with a named escape hatch for bisection
(`CRATONVM_NO_<FEATURE>`), the way `precise_reg_spill_disabled` was rewritten.
An opt-in flag is acceptable only as a temporary bring-up state, and it must be
recorded as *not shipped* until it is inverted. Correspondingly, **any doc
sentence describing a capability must name its default explicitly** — that is
the single cheapest defence against this whole class of drift.

## Data Flow

```
.class bytes
    |
    v
  reader::read_class()     parse into ClassFile
    |
    v
  ClassManager::load_class()  link, verify, prepare
    |
    v
  interpreter::execute()   run bytecode
    |   ^
    |   | invocation / back-edge counts (TieredCompilationManager)
    v   |
  jit_bridge               try_jit_upgrade_with_gate / try_osr
    |                      (background_compile_task by default)
    v
  compile_gate::admit      one door for every backend entry
    |
    v
  try_compile_with_invokespecial_resolver     IR tier, falling back to
  x64::compile_with_param_slots               the single-pass emitter
    |
    v
  JitCache::put / put_osr  install
    |
    v
  execute_jit_call         run compiled code; helpers call back into the VM
    |
    +--> i64::MIN deopt:  deopt_resume  -> interpreter (precise resume or re-run)
    +--> OSR exit:        transfer_osr_exit_into_live_frame_checked -> interpreter
```

## Testing Strategy

- **Unit tests** live in `#[cfg(test)]` modules alongside production code.
- **Integration tests** in `vm/tests/` run compiled `.class` files through
  the full VM pipeline.
- **Java test classes** in `test_classes/` and `vm/tests/resources/` are
  compiled by `build.rs` if `javac` is available.
- **CI** (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo build`,
  `cargo clippy`, and `cargo test` across the workspace on Linux and Windows.
