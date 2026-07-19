# Binary Trees depth-18 half-gap closure (2026-07-18)

Status: fixed.

## Goal and acceptance

The published isolated row was HotSpot 188 ms versus CratonVM 3,855 ms,
or 20.5x. The goal was to cut that gap by at least half: 10.25x or better,
with the exact depth-18 checksum `68332206`.

Final acceptance used `bench/BinTreesClassic.java`, `-Xmx8g` on both VMs,
fresh processes alternating HotSpot and CratonVM, and `taskset -c 13` on the
Azure EPYC benchmark host. No sample was discarded:

| VM | Seven reported times (ms) | median |
|---|---|---:|
| Temurin JDK 25.0.3 C2 | 175, 177, 176, 176, 191, 176, 175 | 176 |
| CratonVM default | 1,479, 1,463, 1,487, 1,466, 1,468, 1,465, 1,474 | 1,468 |

The final gap is **8.34x**, 59.3% below the published 20.5x gap and safely
inside the 10.25x target. All fourteen executions produced `68332206`.
Host load was 2.64 before and 2.50 after the sweep.

## Root causes

`perf` and `CRATONVM_DBG_GCPHASE` isolated two independent costs:

1. The JIT-safe young collector is non-moving, but it inherited the moving
   collector's 50% occupancy trigger. Binary Trees therefore paid two O(heap)
   mark/sweep cycles even though an in-place sweep needs no to-space survivor
   headroom. A representative baseline run spent 1,145 ms in two collections.
2. `Node(Node left, Node right)` is emitted as an inline constructor body.
   Its two reference stores therefore bypassed the existing top-level
   `putfield` fast path and called `jit_putfield_object` twice for every node.
   The helper and its layout/barrier checks were about 17% of the profile.

## Fix

- The generational heap exposes a JIT-allocation-specific trigger query.
  The compiled allocation/refill helper calls it while its compiled caller is
  still discoverable by root gathering, which guarantees the default
  non-moving young cycle. That path triggers at 90% occupancy. Ordinary,
  `--nojit`, explicitly moving-young, and forced-moving paths keep the
  configured 50% copying headroom.
- Inline-site metadata now carries each resolved compact field's byte offset
  and reference classification into code generation.
- Compact reference `putfield` is default-on, with
  `CRATONVM_NO_JIT_INLINE_PUTFIELD` as the opt-out.
- Inline constructor bodies use the same guarded compact store as top-level
  bytecode. For the first syntactic store to a field in verifier-admissible
  `<init>` code, the freshly allocated field is known null and in bounds, so
  the hot arm needs only compact-layout and old-generation checks before the
  bare pointer store. Legacy objects, old receivers, repeated stores, missing
  layout metadata, and all opt-out cases use `jit_putfield_object`.
- The two header-flag checks use direct byte-memory tests, removing redundant
  load/and pairs from the per-node hot path.

An experimental 50/50 active-young split was slower and was reverted. The
final heap geometry remains unchanged.

## Validation

- Release build succeeded; seven-pair acceptance binary:
  `/data/data/cratonvm-binaries/cratonvm-bt-halfgap-019f757f-final4`
  (`sha256 1bafb5abe4e729055836a2882bc32e36e44de97922dd693f06d3663b357bdfd6`).
- After rebasing onto `origin/dev` `c2e358bce`, the exact rebased source built
  as `/data/data/cratonvm-binaries/cratonvm-bt-halfgap-019f757f-eb166bcf5`
  (`sha256 3b5186b5b0b0c08e5ba0cf360c7a7a0a8e2fb412c5df0b1e813e3c731bd04d24`).
  A second five-pair sweep of that binary gave HotSpot median 183 ms and
  CratonVM median 1,479 ms, or **8.08x**, with five exact checksums.
- `cargo test -p cratonvm-gc --lib -- --test-threads=1`:
  792 passed, 0 failed. This includes the moving collector's
  `stress_fill_young_and_promote` capacity regression.
- `cargo test -p cratonvm-jit --lib -- --test-threads=1`:
  912 passed, 0 failed.
- `cargo test -p cratonvm-vm --lib -- --test-threads=1`:
  2,221 passed, 8 failed, 111 ignored. The eight failures are the starting
  `dev` baseline's seven stale `jit::skip_list` expectations plus
  `buffered_input_stream_real_jdk_uses_its_own_bytecode`; the skip-list file
  and the BufferedInputStream override are untouched by this change.
- Correct checksums under JIT and `--nojit`, compact-layout opt-out,
  inline-`putfield` opt-out, genuinely moving young
  (`CRATONVM_MOVING_YOUNG=1 CRATONVM_ALLOW_MOVING_YOUNG=1`), and a
  16 MiB GC-stress trigger.
- The acceptance sweep itself is seven exact depth-18 JIT checksums.

Repository-wide `cargo fmt --all -- --check` remains red on unrelated
pre-existing formatting throughout `dev`; the changed diff passes
`git diff --check`.

## Remaining gap

The remaining 8.34x is dominated by allocation and template-JIT structure
that this focused change does not attempt to redesign: per-object header
publication, frame-slot traffic from the lack of general register allocation,
and no escape/scalar-replacement pass comparable to HotSpot C2.
