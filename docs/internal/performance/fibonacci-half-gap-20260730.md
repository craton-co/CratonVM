# CratonBench Fibonacci half-gap closeout (2026-07-30)

## Acceptance

Goal: reduce the `CratonBench fib` wall-time gap between CratonVM and HotSpot
by at least 50%, with every run returning the exact Fibonacci(44) checksum
`701408733`.

The acceptance calculation is:

```text
old_gap = median(baseline) - median(HotSpot)
new_gap = median(final)    - median(HotSpot)
gap_reduction = 1 - new_gap / old_gap
```

Final alternating fresh-process evidence from the post-merge fat-LTO build is
recorded below.

## Root cause

The hot recursive body already used guarded direct self-calls, but every
invocation still crossed into Rust to publish the precise-map frame base.
Each recursive return then crossed into Rust again to restore the caller's
frame base. Fibonacci therefore paid three `jit_frame_record` helper calls per
non-leaf invocation. Linux had no counterpart to the existing Windows
single-instruction TLS publication and always used this helper path.

The direct self-call safepoint also copied the full GPR set even though the
Fibonacci frame has no live object references. Under default moving-young this
elision had been disabled categorically rather than based on an exact empty-root
proof.

## Fix

- Linux x86-64 now derives the signed displacement of a Rust TLS `Cell` from
  `fs:[0]`, accepts it only after a sentinel written through `Cell` is read back
  through the candidate `fs:[disp32]`, and otherwise retains the helper fallback.
- JIT prologues publish RBP with `mov fs:[disp32], rbp`.
- The separate OSR trampoline emitter uses the same centralized segment-prefix
  selector. An intermediate all-phase sweep caught its historical hard-coded
  Windows `gs:` byte on Linux before push; the final build uses `fs:` in both
  emitters.
- Direct JIT-to-JIT returns republish the caller RBP with the same one-instruction
  TLS store, preserving RAX and replacing the old push/call/pop sequence.
- VM-side precise-root access reads and writes the exact same TLS `Cell`.
- Direct self-calls may publish a metadata-only empty precise map under moving
  young only when dataflow coverage is complete and the exact live-oop home set
  is empty. Any reference or incomplete proof retains the established full
  spill and shadow publication.

The TLS displacement is process-cached but thread-invariant; each thread owns a
distinct `Cell`. Unit tests prove round-trip behavior, identical displacement,
zero initialization in a new thread, and isolation from the parent thread.

## Generated-code evidence

Candidate disassembly for `CratonBench.fib` selected `fs:[0xffffe910]` and
contained exactly three hot TLS stores:

```text
24:  64 48 89 2c 25 10 e9 ff ff    mov [fs:0xffffffffffffe910],rbp
225: 64 48 89 2c 25 10 e9 ff ff    mov [fs:0xffffffffffffe910],rbp
2fe: 64 48 89 2c 25 10 e9 ff ff    mov [fs:0xffffffffffffe910],rbp
```

They are the prologue publication and the two post-recursive-call
republications. The prior `push rax; ...; call jit_frame_record; ...; pop rax`
blocks are absent. The same run returned `701408733`.

With `CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD=1`, Fibonacci(44) completed with
the exact checksum and zero mirror mismatches.

## Correctness evidence

- Linux TLS focused tests: 2 passed, 0 failed.
- Moving empty-root fail-closed predicate: 1 passed, 0 failed.
- GC library: 872 passed, 0 failed.
- Binary Trees moving-GC stress: three fresh processes, `-Xmx512m`,
  `CRATONVM_DBG=moving-young-verify`; all three returned `68332206` with no
  verifier error (times 6,588 / 6,665 / 6,434 ms on the loaded shared host).
- The exact merged final repeated Binary Trees with the moving verifier at
  `-Xmx512m`: `68332206`, no verifier error.
- Final seven-phase CratonBench checksum sweep: PASS for arithmetic, fib,
  sieve, matrix, hashmap, stringregex, and bintrees. Because the host was
  loaded, this used one repetition and a deliberately non-gating 1000%
  performance tolerance; the wrapper still enforced every exact checksum.
- The first merged sweep failed four OSR-heavy phases and its direct Arithmetic
  rerun proved the OSR trampoline still emitted `gs:` on Linux. Centralizing
  the segment prefix fixed it; final Arithmetic returned
  `5000000003999999995` and the seven-phase sweep above passed.
- JIT library: the task-focused tests pass. The full serial library run reaches
  the pre-existing `live_monitor_ops_execute_direct_runtime_stubs` executable
  test and receives SIGSEGV; the exact test fails identically with
  `CRATONVM_NO_PRECISE_INLINE_FRAME_RECORD=1`. Its test helper also wires
  `frame_record=0` and contains no self-call, so neither changed code path is
  emitted.
- Windows release `cargo check -p cratonvm-jit -p cratonvm-vm`: passed. The
  fixture used Community `vcvars64`, retained its MSVC `INCLUDE`, and prepended
  libffi's `include/msvc`, `libffi/src/x86`, and `libffi/include` roots.

## Performance evidence

The shared 16-vCPU host contained unrelated multi-day processes and never
reached the performance gate's required load below 2. Those processes were not
interrupted. Acceptance therefore used five balanced cycles on CPU 15 with
order `HBC / BCH / CHB / HCB / CBH`, fresh processes, `-Xmx8g`, no discarded
samples, and both benchmark wall time and `/usr/bin/time` process CPU time
recorded. Load at each process start was 14.98–16.71, but every arm received
97–99% scheduled CPU while running.

| Arm | Five wall-time samples (ms) | Median |
|---|---|---:|
| HotSpot JDK 25 | 2,576 / 2,485 / 2,494 / 2,085 / 2,013 | 2,485 |
| Baseline CratonVM | 22,493 / 14,748 / 20,129 / 17,187 / 17,106 | 17,187 |
| Fibonacci candidate (pre-OSR audit) | 5,695 / 8,959 / 8,953 / 5,664 / 6,805 | 6,805 |

Every one of the 15 processes returned `701408733`.

```text
old_gap = 17187 - 2485 = 14702 ms
new_gap =  6805 - 2485 =  4320 ms
gap_reduction = 1 - 4320 / 14702 = 70.62%
```

The requested reduction was 50%; this pre-merge candidate exceeds it by 20.62
percentage points.

The exact same five-cycle ordering was then repeated against the fat-LTO binary
built after merging current `origin/dev` (`276cac509`) into the task branch:

| Arm | Five wall-time samples (ms) | Median |
|---|---|---:|
| HotSpot JDK 25 | 1,921 / 1,709 / 2,178 / 1,653 / 1,535 | 1,709 |
| Baseline CratonVM | 14,323 / 15,045 / 15,089 / 13,914 / 12,435 | 14,323 |
| Merged final CratonVM | 5,749 / 5,253 / 6,003 / 5,025 / 4,791 | 5,253 |

Again, all 15 checksums were exact. Start load was 10.91–12.79 and scheduled
CPU was 97–100%.

```text
old_gap = 14323 - 1709 = 12614 ms
new_gap =  5253 - 1709 =  3544 ms
gap_reduction = 1 - 3544 / 12614 = 71.90%
```

The merged final therefore exceeds the requested reduction by 21.90 percentage
points. The absolute numbers are deliberately not used to re-anchor the quiet-
host performance table; the balanced relative acceptance is valid for this
large effect, while the repository's normal gate correctly refuses to calibrate
under this host load.

## Artifacts

All binaries were fat-LTO release builds from the isolated task worktree and
had task-unique names. The pre-merge baseline was:

```text
5799fed95bee55eabb39c52cc11ce9109ca3c82c0ab02dcb0a07ce8e35b469ef
  cratonvm-fibonacci-halfgap-019fb303-baseline
```

The diagnostic Linux-TLS Fibonacci candidate (not the final; the later
all-phase audit fixed its OSR segment prefix) was:

```text
4247607f9fcb96ec364b17c5b3007b1fa56e9d3154c097e4e5d0c86c557e1172
  cratonvm-fibonacci-halfgap-019fb303-candidate4-tls-tests
```

The post-merge final was:

```text
fb2d7fb54ec6ae44fa055602c660123485bda676c90e413e0d0040fd84a7c66a
  cratonvm-fibonacci-halfgap-019fb303-final2-4bcfcc3bc
```
