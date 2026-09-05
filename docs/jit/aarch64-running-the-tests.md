# Running the aarch64 JIT tests

Nothing in this repository executes aarch64 natively: the Windows host, the
Azure build host and `craton` are all x86-64, and `azvm` does not answer. Until
2026-09-04 that meant the aarch64 backend had never been compiled *as* aarch64,
let alone run — its `#[cfg(target_arch = "aarch64")]` code was type-checked only
by temporarily removing the `cfg`, and the machine code it emits had never
executed anywhere.

An emulated arm64 container closes that gap.

## One-time image

```
printf 'FROM rust:1-slim-bookworm\nRUN apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*\n' > Dockerfile.arm64
docker build --platform linux/arm64 -t cratonvm-arm64-dev -f Dockerfile.arm64 .
docker volume create cratonvm-arm64-target
```

`libssl-dev` is not optional: without it the link fails with
`cannot find -lssl`, several minutes into an emulated build.

## Run

```
docker run --rm --platform linux/arm64 \
  -v '<REPO>:/src:ro' -v cratonvm-arm64-target:/target \
  -e CARGO_TARGET_DIR=/target -w /src cratonvm-arm64-dev \
  cargo test --locked -p cratonvm-jit --lib
```

The named volume matters — an emulated build from cold is slow, and reusing it
makes later runs minutes rather than tens of minutes. Mount the repo read-only;
cargo writes only to the target volume and its own `CARGO_HOME`.

## What it proved, and what it found

`aarch64_backend::tests::arm64_execution` is the part that could not exist
before: it compiles a method, publishes the artifact, and CALLS it. Three tests,
all passing —

* a leaf `iadd` executes and returns the right value;
* a CLEAR safepoint flag skips the slow path (the `MOVZ/MOVK; LDRB; CBZ`
  sequence runs, not merely encodes);
* a SET flag reaches the slow path **and the arguments survive it** — the
  regression test for the entry poll having once been emitted before
  `compile_pass` copies the arguments out of X0-X7.

Getting there required fixing what the first run exposed. `cargo test
-p cratonvm-jit` had never been runnable on aarch64:

* five tests referenced `#[cfg(target_arch = "x86_64")]` APIs
  (`first_unsupported_precise_frame_site`, `CompiledMethod::osr_enter`) and so
  did not COMPILE;
* `jit/src/x64/tests.rs` and `ir_lower`'s test module EXECUTE the x86-64 code
  they emit, which on aarch64 is an illegal instruction that killed the whole
  test binary (`SIGILL` at
  `ir_lower::tests::a_previously_declined_large_method_now_compiles`) — they are
  gated at the module now, because the property is "this module is about
  x86-64", not a per-test accident;
* thirteen more assert how `try_compile_inner` ROUTES a compile, which on
  aarch64 returns early through the Arm64 branch before any routing happens.

After that: **1792 passed, 0 failed.**

## Caveats

This is qemu, not a machine. Instruction *semantics* are emulated faithfully
enough to trust the arithmetic and control flow above, but it is not a timing,
memory-ordering or errata oracle, and self-modifying code is exactly where
emulation is weakest — the execution tests take a mutex partly because
concurrent write-then-execute of JIT buffers produced a spurious `SIGILL` under
qemu that did not reproduce serially.

Nothing here exercises the GC: these are unit tests of the jit crate, so the
safepoint polls, oop maps, coverage claim and the armed verify oracle still have
not run against a live collector. That needs the whole VM built for aarch64 and
a Java workload — feasible in the same container, but an order of magnitude more
emulated build time.

## Running the whole VM on AArch64 (2026-09-04)

The unit tests above exercise the jit crate. Running the VM itself — and with it
the safepoint polls, oop maps, coverage claim and the armed verify oracle
against a live collector — needs the binary, and until 2026-09-04
`cratonvm-cli` had never been built for AArch64 at all.

**Six source errors stood in the way, all in `cratonvm-vm`, all portability
rather than design:**

| what | where |
| --- | --- |
| `core::arch::x86_64::_rdtsc()` ungated (×2) | `vm/src/jit/helpers.rs` |
| `asm!("mov {}, rbp")` ungated | `vm/src/jit/helpers.rs` |
| `first_unsupported_precise_frame_site` (x86-only) called unconditionally | `vm/src/runtime/interpreter/jit_bridge.rs` |
| `osr_enter_planned` (x86-only) called unconditionally | `vm/src/runtime/interpreter/jit_bridge.rs` |
| `dlopen(.. as *const i8)` — `c_char` is `u8` on AArch64 | `vm/src/vm/vm_exec.rs` |

The counter now reads `CNTVCT_EL0` on AArch64 and the frame pointer `x29`; the
two OSR sites are gated, because no backend but x86-64 publishes OSR entry
points. Two system libraries are also needed beyond the jit crate's:
`libffi-dev` (or autotools, to bootstrap libffi) and `libxcb1-dev`.

Add to the image:

```
libffi-dev libxcb1-dev libx11-dev build-essential automake autoconf libtool texinfo cmake
```

and an AArch64 JDK, e.g.
`curl -sSL https://api.adoptium.net/v3/binary/latest/25/ga/linux/aarch64/jdk/hotspot/normal/eclipse`.

### What it showed

`cratonvm` runs a Java program on AArch64 and gets the right answer, and the
collector runs under it — 7 collections on a 48 MB heap.

**But the JIT machinery is still NOT exercised, and the oracle's silence is
therefore vacuous.** With `CRATONVM_JIT_METRICS_OUT` on, the tier-up path
records two compilation attempts for the one method shaped for this backend
(`work`, a leaf arithmetic loop) and **both come back
`"outcome":"abandoned"`** — the backend refuses it. Nothing is compiled, so no
poll executes, no map is published, no coverage claim is made and the oracle has
nothing to refute.

So the state is: the VM is portable, the collector runs, and the remaining
blocker is a single named refusal. **The next step is to find why `work` is
abandoned** — `CRATONVM_JIT_METRICS_OUT`'s record for it is where to start, and
that is a much smaller question than "does any of this work on AArch64".
