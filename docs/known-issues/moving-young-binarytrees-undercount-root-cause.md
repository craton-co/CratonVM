# Moving-young binaryTrees under-count root cause

Status: OPEN. Root-caused; no production fix in this change.

## Summary

The reproducible live-object under-count I can trigger is not in
`jit/src/x64.rs::collect_live_oop_homes`. It is an OSR-frame coverage hole made
unsafe by `CRATONVM_MOVING_YOUNG=1`.

The missed oop is `BenchSuite$Node longLived` in local slot 6 of the
OSR-entered frame for `BenchSuite.binaryTrees(I)J`. When a moving young
collection happens while that OSR frame is live, the `longLived` root is not
published to the shadow stack and therefore is not rewritten after Cheney moves
the tree. The final `check(longLived)` then sees the stale from-space copy as a
leaf and contributes `1` instead of `131071` for depth 16.

This is a root/home miss, not a heap rewrite miss:

```text
RESULT name=bintrees16 checksum=14854832
[moving-young-verify] forwarded_heap_refs_remaining young=0 old=0 ...
```

The checksum delta is exact:

```text
14985902 - 14854832 = 131070 = itemCheck(depth16Tree) - 1
```

The confirming OSR trace shows `binaryTrees` entered via OSR with local slot 6
holding the `longLived` object pointer:

```text
[cratonvm-osr] enter BenchSuite.binaryTrees(I)J entry_pc=51 num_locals=12 locals=[16, 262143, 0, 4, 262143, 0, 767235208, 4, 65536, 62000, 0, 2001]
```

## Mechanism

`CRATONVM_MOVING_YOUNG=1` changes the GC/JIT contract:

- `gc/src/gen_heap.rs` allows moving Cheney even when JIT frames are active by
  setting `divert_non_moving = (has_conservative_roots && !moving_young) || ...`.
- `vm/src/memory/roots.rs` suppresses
  `conservative_roots::scan_active_jit_frames` when moving young is enabled.
- Correctness then depends on every live JIT-held oop being in a rewritable
  shadow-stack home.

Whole-method compiled frames do publish their homes through
`collect_live_oop_homes`. The reproduced failure happens before that path can
cover the frame: `jit/src/lib.rs::emit_osr_trampoline` deliberately disables
shadow tracking for OSR-entered frames unless `CRATONVM_SHADOW_OSR_TRACK=1`.
In the default branch it zeroes the compiled frame's cached thread slot:

```text
else if shadow_thread_slot_off != 0 {
    MOV qword [rbp - shadow_thread_slot_off], 0
}
```

That makes generated `emit_shadow_push`/`emit_shadow_reload` null-guard out for
the OSR frame. Under a non-moving young collection this is safe because the
conservative scan pins/marks the raw stack word. Under the moving-young gate the
conservative scan is suppressed, so local slot 6 is neither pinned nor rewritten.

`CRATONVM_SHADOW_OSR_TRACK=1` flips the OSR trampoline to store the real
`JvmThread*` and save the shadow-stack watermark; with that extra gate, the same
run returns the correct checksum.

## Minimal Repro

Compile the minimal repro:

```powershell
javac -d scratch\moving-young-bt docs\known-issues\repros\MovingYoungBtOsr.java
```

Run with OSR enabled:

```powershell
$env:CRATONVM_MOVING_YOUNG='1'
$env:CRATONVM_JIT_OSR='1'
.\cvmmybtrootcause-20260701.exe --stack-dump-on-timeout=0 -Xmx4g -cp scratch\moving-young-bt MovingYoungBtOsr 16
```

Observed on both the current diagnostic branch (`397ebadd` base) and an exact
`437dfed6` diagnostic worktree:

```text
14854832
```

Expected:

```text
14985902
```

Control runs:

```text
CRATONVM_MOVING_YOUNG=1                         -Xmx4g -> 14985902
CRATONVM_MOVING_YOUNG=1 CRATONVM_JIT_OSR=1      -Xmx4g -> 14854832
CRATONVM_MOVING_YOUNG=1 CRATONVM_JIT_OSR=1 CRATONVM_SHADOW_OSR_TRACK=1 -Xmx4g -> 14985902
```

The post-Cheney verifier added under `CRATONVM_MOVING_YOUNG_VERIFY=1` reported
`forwarded_heap_refs_remaining young=0 old=0 pointer_map=254812` for the
standalone `-Xmx4g` under-count run, so this is not a missed heap reference
rewrite. Smaller heaps can crash before producing the final checksum, which is a
noisier manifestation of the same stale OSR root.

## Attribution

The failure is a latent OSR coverage hole exposed by the moving-young gate. It
is not explained by the three suspected `collect_live_oop_homes` branches
(`i >= 64`, `local_oop_reached[cur_bc_pc]`, or stack/local dataflow) in the runs
above; default OSR-off moving young stayed correct on `437dfed6` and on the
current local dev branch.

The design docs already state the intended safety rule: OSR-compiled methods
are excluded from `fully_oop_covered` (`!cm.compiled_via_osr`) so a future moving
collector should pin or fall back for them. The gated moving-young path does not
enforce that coverage guard; it suppresses the conservative scan and moves
anyway. That is the bug.

Relevant references:

- `docs/feature-designs/default-moving-young-gen.md`
- `docs/feature-designs/precise-jit-maps-default.md`
- `docs/internal/app-jvm-bugs/jit-osr-main-corruptor-investigation.md`

Do not fix this by enabling `CRATONVM_SHADOW_OSR_TRACK` blindly. The production
fix should add a moving-young coverage-completeness guard and divert to the
non-moving fallback whenever any active JIT frame is not safely rewritable
(including OSR frames), then separately decide whether precise OSR shadow
tracking is mature enough to remove that fallback for OSR.
