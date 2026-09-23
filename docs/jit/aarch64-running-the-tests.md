# Running the aarch64 JIT tests

Nothing in this repository executes aarch64 natively: the Windows host, the
Azure build host and `craton` are all x86-64, and `azvm` does not answer. Until
2026-09-04 that meant the aarch64 backend had never been compiled *as* aarch64,
let alone run — its `#[cfg(target_arch = "aarch64")]` code was type-checked only
by temporarily removing the `cfg`, and the machine code it emits had never
executed anywhere.

An emulated arm64 container closes that gap.

## A faster route than the container (2026-09-21)

The container below still works and is what the whole-VM section needs. For
the **jit crate's own tests** there is now something an order of magnitude
faster, because nothing has to be emulated except the test binary itself:
cross-COMPILE on the x86-64 host and run the result under `qemu-user`'s
`binfmt_misc` handler.

One-time, on the Azure build host (`/data/CratonVM`):

```
sudo apt-get install -y qemu-user-static gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
# libssl, for the openssl-sys in cratonvm-jit's dependency tree
# (native-tls <- cratonvm-native-api <- cratonvm-jit-api <- cratonvm-jit)
sudo dpkg --add-architecture arm64
# ports.ubuntu.com, not azure.archive: the main archive carries no arm64.
# Pin the existing sources to amd64 first or `apt-get update` fails on them.
sudo apt-get install -y libssl-dev:arm64
```

`qemu-user-static` registers the `binfmt_misc` handler as part of its install,
so an aarch64 ELF then runs by being executed.

Per run:

```
export CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER="qemu-aarch64-static -L /usr/aarch64-linux-gnu"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
export PKG_CONFIG_SYSROOT_DIR=/
cargo test --locked -p cratonvm-jit --lib
```

A cold build is a few minutes; a warm one plus the full 2 746-test run is
about seven seconds. **Put `--target` in the environment, not on the command
line after a `--`** — `cargo test ... -- --test-threads=1 --target aarch64-…`
passes the target to the test harness and builds for the host, which then
fails to link against the arm64 `libssl`.

The same caveats as the container apply: this is qemu, not a machine, and in
particular it is not a memory-ordering or errata oracle.

### What it found the first time it was run to completion (2026-09-21)

The suite had never finished on aarch64. Three things stood in the way, all
fixed in the same change:

* **The crate did not COMPILE.** `jit/src/lib.rs`'s
  `#[cfg(not(target_arch = "x86_64"))]` RBC.6 arm called `jitc_bail!`, which
  is not in that function's scope.
* **Three test modules in `jit/src/x64/objects.rs` EXECUTE the x86-64 bytes
  they emit**, which on aarch64 is a `SIGSEGV` that kills the whole test
  binary — so every test after it silently never ran. They are gated at the
  module now.
* **Two round-9 routing tests** in `jit/src/lib.rs` assert how
  `try_compile_inner` routes a compile, which on aarch64 never happens. Gated,
  like the thirteen siblings gated in 2026-09-04.

`jit/tests/panic_free_compile_ratchet.rs` needed a matching change: its
"non-test code" scanner stopped only at the bare `#[cfg(test)]`, so a module
gated `#[cfg(all(test, target_arch = ...))]` was scanned as production.

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

### Since 2026-09-22: qemu-user cross-builds the whole VM too, in nine minutes

The container above is no longer the fast route for the VM either. With the
four cross-compile environment variables from the section below, plus
`libffi-dev:arm64 libxcb1-dev:arm64 libx11-dev:arm64` and an AArch64 JDK,

```
CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu \
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
PKG_CONFIG_ALLOW_CROSS=1 \
PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
  cargo build -p cratonvm-cli --release
```

produces a real AArch64 `cratonvm` in about nine minutes on the Azure host, and
binfmt runs it:

```
JAVA_HOME=<aarch64 jdk> CRATONVM_JIT_ARM64=1 \
  ./target/aarch64-unknown-linux-gnu/release/cratonvm --cp <dir> <MainClass>
```

Two things to get right, both of which cost a wasted run otherwise.

* **Size the workload for the SHUTDOWN, not for the clock.** The oop-map
  oracle's tally is printed by `oop_map_audit::dump()` from `vm-cli`'s normal
  shutdown path. A run killed by `timeout` prints nothing at all, however much
  it did. Under qemu with `CRATONVM_DBG_GC_STRESS=1` a few thousand iterations
  is plenty; a few hundred thousand is a run you will never see the numbers
  from.
* **The tally is self-gated on having looked.** `dump()` returns silently when
  it inspected no frames, so silence means "the oracle never engaged", NOT
  "nothing was wrong". Read `frames=` before reading `never_mapped=`.

## Since 2026-09-12: the backend is opt-in, and the execution tests assert Java semantics

A review of the backend (see `docs/jit/aarch64-parity.md`) found it miscompiled
ordinary leaf methods: deep expressions, `float` arithmetic, `freturn`/`dreturn`,
32-bit `int` wrapping, NaN compares, and parameters after a `long`/`double`.
Those are fixed in code, **but none of the fixes has run on an AArch64 host
yet.** Two consequences for anyone running this:

* **The VM no longer uses the backend unless `CRATONVM_JIT_ARM64=1` is set.**
  A whole-VM run like the one above must set it, or the JIT is simply off and
  the run proves nothing about compiled code.
* `arm64_execution` grew. The two tests that pinned the 64-bit `iadd`/`ishl`
  answers now assert the JVMS ones (`iadd_wraps_at_32_bits_as_the_jvms_requires`,
  `ishl_masks_the_shift_to_five_bits`), and new ones execute the
  `a - (b+1+2+3+4)` repro, a `double` parameter through `dreturn`, and
  `fcmpg`/`fcmpl` of NaN. The three tests the CI job names are unchanged. The
  host-independent tests in the same module include `eval_int_method`, a small
  pseudo-op evaluator that checks VALUES on any host -- a stopgap, not a
  substitute for running these.
