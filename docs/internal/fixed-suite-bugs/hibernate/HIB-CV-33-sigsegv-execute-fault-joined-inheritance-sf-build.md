# HIB-CV-33 — SIGSEGV ("execute" access violation) building a JOINED-inheritance SessionFactory

> **✅ FIXED on dev (`c9258e17`, branch `fix/gc-young-sweep-corruptor`, 2026-06-23).**
> The non-moving young sweep under `promotion_oom_risk` corrupted live young
> objects *because it was being run without conservative JIT roots to protect*. The
> sweep's safety rests on conservative over-marking of the un-rewritable JIT
> register/spill slots a moving Cheney cannot relocate; with no JIT frame on the
> stack (`--nojit`, or JIT-quiescent) there are none, so its precise-root/remap path
> reclaimed a still-live young object. Fix (`gen_heap.rs`): only honor
> `promotion_oom_risk` as a divert-to-non-moving reason when conservative roots are
> actually present; otherwise the precise moving collector runs (its fresh to-space
> always holds the packed live set, so the abort this heuristic guarded against is
> unreachable) **and** the Phase-5 major GC — previously skipped by the non-moving
> early-return — relieves old-gen pressure. == `FORCE_MOVING` on the `--nojit` path
> (proven clean), JIT-active bt18-tuned path untouched. Opt-out
> `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`. **HIB-CV-22 and HIB-CV-32 are the same
> corruptor (different victims) and are resolved by this one fix.** Verified:
> `scratch/h22repro/NatPressure` (fix == `FORCE_MOVING` clean / old fails),
> bt16/bt18 == HotSpot under `--nojit` (14985902 / 68332206) **and** JIT, 737 GC
> unit tests.

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `f8cdd52b`, worktree `C:/craton/CratonVM-hibtest`); reconfirmed on dev `71c0f65d`
**Severity:** High — hard VM crash; HotSpot PASS
**Status:** **ROOT-CAUSED** — generational/G1 **non-moving young-gen sweep corrupts the live heap** under heap pressure. **NOT** deterministic, **NOT** stack-overflow, **NOT** a metamodel-recursion bug. Distinct from HIB-CV-32 (verified).

> **Correction to the original triage.** The first write-up called this
> "deterministic, reproduces under `--nojit`, poss. uncaught stack overflow from
> re-entrant `createSessionFactory`." Measurement shows otherwise: it is a
> **load/timing-sensitive heisenbug** caused by the GC, the "re-entrant"
> `createSessionFactory` is normal JUnit lazy-init (not recursion), and forcing
> the moving collector or a large heap makes it vanish. Details below.

---

## Symptom

`org.hibernate.orm.test.mapping.inheritance.joined.JoinedInheritanceSameAttributeNameTest`
crashes the VM (SIGSEGV, rc=139) somewhere during the SessionFactory/transaction
work. Captured fatal dump:

```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF78A806AAC
#  Faulting access: execute at address 0x00007FF78A806AAC
#  thread: "main-vm"
#  exe module base: 0x00007FF789610000
#  faulting RVA: 0x11F6AAC
Native frames (most recent call first) [raw]:
   0: exe+0xB135D9
   1..3: external/jit          (actually system DLLs — anything outside .text is labelled "external/jit")
   4: exe+0x11F6AAC            (faulting; ~18.8 MB into a 21.5 MB exe → past .text, non-executable)
   5: 0x0000000025973010       (a HEAP address sitting on the call stack — a corrupted return address)
```

HotSpot: PASS. The Java stack at the dump is the ordinary JUnit lazy-init chain
`createSessionFactory → getSessionFactory → inTransaction → …` (the test's
`@BeforeAll`/`inTransaction` triggers a one-shot lazy `createSessionFactory`); it
is **not** unbounded recursion — many runs complete this exact path fine.

The model uses `@Inheritance(JOINED)` + sub/superclass sharing an attribute name
+ `@ElementCollection`/`@Embeddable`. That schema only matters insofar as it makes
the test **allocate heavily** during SessionFactory build; it is not where the
bug lives.

## Root cause — GC non-moving young sweep corrupts a live object

The fault is a **corrupted code/return pointer** (execute access violation at a
non-`.text` RVA, with a heap address found on the native call stack), i.e. the
heap/stack was already corrupted before the crash. The corruptor is the
**generational collector's non-moving young-gen sweep** (`sweep_young_non_moving`,
`gc/src/gen_heap.rs`).

CratonVM picks the non-moving sweep over the moving (Cheney) young collector when
any of these hold (`gen_heap.rs:2521`):

```rust
if (gc_quiescence::is_active()                       // a JitEntryGuard is live
    || gc_quiescence::unregistered_jit_frame_on_stack()  // a JIT code addr on the native stack
    || promotion_oom_risk)                            // BOTH gens ≥ 90% full
    && !force_moving
{
    self.sweep_young_non_moving(...)
}
```

Under `--nojit` the first two are **always false** — there is no compiled code,
no `JitEntryGuard` (`is_active()` counts interpreter→JIT entries), and
`unregistered_jit_frame_on_stack()` is only set when the root scan finds a *JIT
code address* among the native stack words (none exist with the JIT off). So the
**only** way the non-moving sweep runs under `--nojit` is the
**`promotion_oom_risk`** branch: both young and old generations momentarily ≥90%
full (`gen_heap.rs:2499‑2514`).

The non-moving sweep was designed and validated for the *conservative-JIT-root*
case, where over-marking + pinning keep everything alive and never relocating is
the whole point. Routing to it merely because the heap is transiently full — with
**no** conservative roots to protect — exposes a latent defect: it reclaims/relocates
a still-live young object (a missed old→young card edge or a selective-promotion
remap that misses an interpreter-frame reference), leaving a dangling pointer.
That pointer is later loaded and used as a call/return target → execute fault.

The moving collector handles the identical root set correctly here (it
successfully evacuates and the test passes), so this is **not** a missing root in
the shared root scan — it is specific to the non-moving sweep path.

## Discriminator matrix (all measured, `--nojit`, `JoinedInheritanceSameAttributeNameTest`)

| Configuration | Crashes | Interpretation |
|---|---|---|
| default heap, **idle** machine | rare (4/4 passed cold) | the 90/90 race is seldom hit when GC keeps up |
| default heap, **loaded** machine | **11/11** | allocation outpaces GC → both gens hit 90% → non-moving sweep → corruption |
| `-Xmx6g -Xms6g` (GC effectively never collects) | **0/5** | gens never reach 90% → always moving → no corruption |
| `-XX:+UseG1GC` | **5/5** | G1's analogous in-place/evac-failure fallback under a full heap corrupts too |
| `CRATONVM_DBG_FORCE_MOVING=1` | **0/5** | bypasses the non-moving sweep entirely (`&& !force_moving`) |
| `RUST_LOG=debug` (very slow) | **0/4**, non-moving sweep never even entered | slow allocation lets the moving collector keep both gens < 90% |

The matrix triangulates the cause precisely: corruption requires **(a)** a GC to
actually run (big heap avoids it) and **(b)** that GC to take the **non-moving
sweep** path (`FORCE_MOVING` avoids it). It is independent of the configured
collector tier — both the default generational collector and G1 crash, because
both fall into a non-moving in-place reclamation under a full heap. Load
sensitivity is the 90%/90% window being a race between mutator allocation and GC.

### Why it looked "fixed on newer dev"
On dev `71c0f65d` the first handful of *cold* runs passed, which can read as
"fixed by the JIT-range-scan commits (`e601c4ba`/`71c0f65d`)." It is not: those
commits only touch `native_stack_has_jit_frame`, which under `--nojit` has zero
JIT ranges and returns immediately on both code paths — behaviorally a no-op with
the JIT off. They merely shifted the binary layout/timing and thus the heisenbug's
probability. Under load, `71c0f65d` crashes 11/11. The bug is present on both
`f8cdd52b` and `71c0f65d`.

## Relationship to other crashes

- **HIB-CV-32 (byte[]→BLOB bind, "read" fault) is genuinely distinct.** Verified:
  CV-32 crashes **5/5 under `CRATONVM_DBG_FORCE_MOVING=1` and 3/3 with `-Xmx6g`** —
  i.e. it is **deterministic and GC-config-independent**, a real fault in the
  `byte[]`→BLOB data path, not the GC corruptor. (The original "distinct" call was
  right, but for the wrong reason — it is not "execute vs read," it is
  "GC-dependent vs not.")
- **HIB-CV-35's "non-SIGSEGV crash variants" and any other flaky `--nojit`
  SIGSEGVs during heavy-allocation Hibernate work are candidates for the SAME GC
  root cause** as CV-33 and should be re-tested with the discriminator matrix
  above before being filed separately.
- This is the **interpreter-mode analog** of the long-running JIT GC-corruption
  thread (conservative roots / parallel-evac / non-moving sweep). See the G1
  parallel-evac persistent-forwarding and self-forward UAF notes, and the
  `sweep_young_non_moving` design comments at `gen_heap.rs:2454‑2545`.

## Reproduce

```sh
cd apps/hibernate-orm/.cratonvm-suite
echo org.hibernate.orm.test.mapping.inheritance.joined.JoinedInheritanceSameAttributeNameTest > list.txt

# Crashes most reliably under load (run a few in parallel, or load the machine):
cratonvm.exe --nojit @common.args CratonRunner list.txt 0      # -> SIGSEGV (execute), rc=139, flaky

# Vanishes with either of these — proves it is the non-moving young sweep:
CRATONVM_DBG_FORCE_MOVING=1 cratonvm.exe --nojit @common.args CratonRunner list.txt 0   # 0 crashes
cratonvm.exe -Xmx6g -Xms6g --nojit @common.args CratonRunner list.txt 0                 # 0 crashes
```

Symbolizing the release dump is unreliable: the release PDB has line-tables only
(no `S_GPROC32` records), so `pdbresolve` returns `<no frames>` and the built-in
`CRATONVM_SYMBOLIZE=` falls back to nearest-export guesses
(`_xmm+0x5C`, `jit_drem+0xAC589`). Build the `profsym` profile
(`gen_heap.rs`/`Cargo.toml` `[profile.profsym]`, full debuginfo) if a named GC
stack is needed.

## Incidental observation (ruled out as the corruptor)

Both crashing **and** passing runs emit exactly one:

```
gen_heap::get_field: out-of-bounds field read dropped (… speculative collection-layout
probe …) class_name=java/lang/Long index=1 num_slots=1
```

A speculative collection-layout probe reads slot 1 of a 1-slot `java/lang/Long`;
the guard catches and drops it. It fires identically in passing big-heap runs, so
it is **not** the corruptor — but the existence of an *unguarded* sibling of this
speculative probe would be worth auditing as a secondary lead.

## Suggested fix direction (needs GC owner + gauntlet soak)

The `promotion_oom_risk` heuristic (both gens ≥90%) diverts to the non-moving
sweep purely to dodge the moving collector's `process::abort()` on a failed
promotion. But:

1. In this scenario the heap is **not** genuinely exhausted — `FORCE_MOVING`
   completes the test cleanly — so the 90/90 trigger is a **false positive** that
   needlessly routes a correct workload through the buggy sweep.
2. The non-moving sweep is only *validated correct* when there are conservative
   roots to pin (JIT). With **no** JIT frames, the moving collector is correct and
   safe.

Two candidate fixes (mutually compatible):

- **(A) Gate the `promotion_oom_risk` diversion on the presence of conservative
  roots.** When `!is_active() && !unregistered_jit_frame_on_stack()`, prefer the
  moving collector even under heap pressure; let a genuinely exhausted heap surface
  a *catchable* `OutOfMemoryError` rather than silently corrupt. (This is exactly
  what `FORCE_MOVING` does, and it is provably safe with the JIT off.)
- **(B) Find and fix the actual liveness/remap defect in `sweep_young_non_moving`**
  for the no-conservative-root case — likely the selective-promotion `pointer_map`
  not covering an interpreter-frame reference, or a dropped old→young dirty-card
  edge (`gen_heap.rs:3624‑3674`). This is the more complete fix but the harder one
  to verify against a heisenbug.

Both are load-bearing GC changes; gate them and run the GC/app gauntlet (bt16/bt18
checksums == HotSpot, kafka/tomcat/Hibernate soak) before flipping defaults.

## Triage

Real, high-severity, GC-induced heap-corruption crash, **independent of the JIT**,
exposed by heavy interpreter-mode allocation under heap pressure. Heisenbug
(load/timing sensitive), not deterministic. Belongs to the GC owner. Likely
covers other flaky `--nojit` Hibernate SIGSEGVs in this run.
