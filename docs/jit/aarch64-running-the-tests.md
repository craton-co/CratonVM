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
