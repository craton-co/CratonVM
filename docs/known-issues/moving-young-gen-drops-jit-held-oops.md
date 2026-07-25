# `CRATONVM_MOVING_YOUNG` silently corrupts the heap when the JIT is enabled

| | |
|---|---|
| **Status** | OPEN — reproduces on unmodified `dev`. The flag is opt-in and OFF by default, so the default build is unaffected. |
| **Severity** | HIGH for anyone who enables the flag: the failure mode is a **wrong, non-deterministic answer**, not a crash or an exception. |
| **Captured** | 2026-07-25, branch `arch/tiers-1-3-20260725` (found while profiling the layout-registry cache work) |
| **Host** | Azure `20.83.144.174`, 16 cores / 31 GB, worktree `/data/data/wt-archtiers-20260725` |
| **Binaries** | `/data/data/cvm-archtiers-base.bin` (dev @ `4ac6e11fd`), `/data/data/cvm-archtiers-mru.bin` (@ `a7accf8df`) — **both** reproduce, so this is not caused by the layout-cache commits |

## Symptom

`CRATONVM_MOVING_YOUNG=1` makes the young generation a copying collector instead
of the default non-moving sweep. Under enough GC pressure it loses or
mis-remaps live references, and the program computes a **wrong result that
varies between runs**.

Reproducer — `bench/BinTreesClassic.java` at depth 18, whose correct checksum is
a documented constant (`68332206`):

```bash
javac -d /data/data/bench-classes-archtiers bench/BinTreesClassic.java
CVM=/data/data/cvm-archtiers-mru.bin
BENCH=/data/data/bench-classes-archtiers

# correct
$CVM --java-home /home/victor/jdk25 -Xmx8g -cp $BENCH BinTreesClassic 18
# CRATONVM_MOVING_YOUNG=1 -> wrong, and different each run
CRATONVM_MOVING_YOUNG=1 $CVM --java-home /home/victor/jdk25 -Xmx8g -cp $BENCH BinTreesClassic 18
```

## Measured matrix

All runs `-Xmx8g`, depth 18 unless stated. Correct checksum is `68332206`.

| Configuration | Time | Checksum | |
|---|---|---|---|
| default (non-moving young) | 2722 / 2724 / 2751 ms | `68332206` | ✅ |
| `CRATONVM_MOVING_YOUNG=1` | 3299 / 3306 / 3291 ms | `68310826`, `68310832`, `68310832` | ❌ wrong, varies |
| `CRATONVM_MOVING_YOUNG=1` + `CRATONVM_ALLOW_MOVING_YOUNG=1` | 8258 / 9016 ms | `68029454`, `68029436` | ❌ worse, varies |
| `CRATONVM_MOVING_YOUNG=1` + `CRATONVM_DISABLE_JIT=1` | 182 / 186 s | `68332206` | ✅ **correct** |
| `CRATONVM_MOVING_YOUNG=1`, depth **14** | 88 / 91 ms | `3222190` | ✅ correct |
| `CRATONVM_MOVING_YOUNG=1` on the older baseline binary | 4417 / 4548 ms | `68311064`, `68310832` | ❌ pre-existing |

## What the matrix establishes

1. **The bug is in the interaction with JIT-compiled frames, not in the copying
   collector itself.** With `CRATONVM_DISABLE_JIT=1` the moving young gen
   produces the correct checksum on every run. Turn the JIT on and it corrupts.
   This matches the code's own caveat at `gc/src/gc_quiescence.rs:245`
   ("`CRATONVM_MOVING_YOUNG` is only sound while every live JIT-held oop is
   …") and the existence of `moving_young_coverage_incomplete()` — i.e. the JIT
   root coverage required to make relocation safe is genuinely incomplete, and
   the consequence is silent rather than loud.
2. **It needs real GC pressure.** Depth 14 is clean; depth 18 is not. So a small
   smoke test will not catch it.
3. **`CRATONVM_ALLOW_MOVING_YOUNG=1` makes it worse, not better.** That flag
   defeats the `fail_closed_non_moving` guard in
   `gen_heap.rs::collect_garbage_inner` (~line 3766), which otherwise diverts
   some collections back to the non-moving path while quiescence is active.
   Removing the diversion means more collections actually move, and the
   checksum drifts further from correct (`68029…` vs `68310…`). This is good
   corroborating evidence that the corruption is proportional to how much
   relocation actually happens.
4. **It is not a regression from the 2026-07-25 layout-cache work** — the
   pre-change baseline binary reproduces it identically.

## Why the checksum is a strong signal

`BinTreesClassic` sums `itemCheck` over a tree it builds and discards. A missed
remap makes one subtree unreachable or points at a stale copy, so the sum comes
out low — and the observed values are all *slightly below* correct
(`68310832` vs `68332206`, a ~0.03% shortfall), which is the shape of "a few
nodes lost per collection" rather than gross structural damage. That it varies
run to run is the tell that it tracks GC timing.

## Suggested next steps

- Do **not** enable this flag, and do not treat it as a performance option: even
  ignoring correctness it measured *slower* than the default here (3299 ms vs
  2722 ms), so there is no throughput argument for taking the risk today.
- The fix is to complete precise root coverage for JIT frames under relocation.
  The relevant machinery already exists and is partially wired:
  `vm/src/jit/conservative_roots.rs:354` notes that `CRATONVM_MOVING_YOUNG`
  "implies the shadow-stack root scan + remap", and there is a
  `CRATONVM_MOVING_YOUNG_VERIFY` gate (`gen_heap.rs:9633`) plus a
  `CRATONVM_MOVING_YOUNG_FALLBACKS` diagnostic (`gen_heap.rs:3786`). Running the
  repro under those should localise which frames are missed.
- Consider making the failure loud instead of silent while coverage is
  incomplete: if `moving_young_coverage_incomplete()` is true and a relocation
  is about to happen with JIT frames live, aborting (or forcing non-moving)
  would be far better than returning a wrong answer to the application.
- Add a checksum-verifying GC-stress case to the regression suite using this
  exact repro, so that completing the coverage work has an objective pass
  criterion.

## Related

- `docs/feature-designs/concurrent-gc-maturation.md` — why Generational remains
  the default safety net.
- The non-moving young sweep is also the current throughput cost centre: after
  the layout-registry cache landed, `gen_object_total_size` (22.1%),
  `sweep_young_non_moving` (4.7%) and `__memset_avx512` (3.1%) were the top
  symbols in a depth-18 profile, because a non-moving sweep walks every object
  rather than only the live ones. A *correct* moving young gen is therefore
  still the right long-term direction — this issue is what blocks it.
