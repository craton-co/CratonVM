# `fully_oop_covered` asserts a map EXISTS, not that it lists every live oop

## What the bit is spent on

`CompiledMethod::fully_oop_covered` is the codegen half of the proof that lets
`memory::roots::collect_roots` skip the conservative JIT root scan
(`moving_young_precise_only`). Skipping it is not a small economy: the
conservative scan is the only producer of G1's pin set and the only thing that
marks a JIT-held oop the maps do not name, so a collector that skips it on a
false proof has no backstop at all.

## What the bit actually checks

```rust
cm.fully_oop_covered = compiler.precise_maps
    && compiler.sp_id_slot_off != 0
    && compiler.inline_sites.is_empty()
    && compiler.safepoint_pcs.is_subset(&compiler.mapped_safepoint_pcs);
```

`safepoint_pcs ⊆ mapped_safepoint_pcs` is a **presence** test: every GC-capable
safepoint recorded *an* entry. It says nothing about whether the entry lists
every live oop at that safepoint. And `mapped_safepoint_pcs.insert(...)` runs
whenever the precise gate is on, before the slot list is examined — so a
safepoint whose map dropped **every** oop still counts as mapped.

Three drops in `emit_oop_map_for_safepoint` are unconditional and silent, and
none of them clears the bit:

| # | the drop | what is lost |
|---|---|---|
| 1 | `if !self.stack_oop_marks[i] { continue; }` | an operand-stack oop the mark vector does not know about — including marks **padded with `false`** when `stack.len() != stack_oop_marks.len()` |
| 2 | `if let StackSlot::Frame(off) = self.stack[i]` | an oop operand still resident in a register/scratch slot: no `else`, nothing recorded |
| 3 | `if let Ok(i16_off) = i16::try_from(off)` | any slot beyond ±32767 from `rbp` |

Drop 1 is the interesting one because the compiler already knows when it has
happened. `stack_oop_marks_exact` is set `false` on desync and IS consulted —
`can_elide_self_call_register_spill` and the shadow-publication predicate both
fail closed on it. `fully_oop_covered` does not consult it. The same fact that
is trusted to veto a spill elision is not trusted to veto the coverage claim.

The emitter's own comment says the padding is safe *because of the backstop*:

> padding with `false` (non-oop) stays SOUND because
> `conservative_roots::scan_one_frame_precise` also conservatively sweeps the
> frame region — but a desync would silently degrade precision (and is unsafe
> for the *moving* path)

That is exactly the backstop the bit is spent to suppress.

## The contract was written down, and it is not honoured

`jit/src/x64/driver.rs`, immediately above the assignment:

> It is a NECESSARY codegen precondition; the runtime
> `CRATONVM_DBG_VERIFY_OOP_MAPS` oracle (Stage G0) is the SUFFICIENT proof that
> must gate the actual backstop suppression before the moving path relies on it.

The oracle gates nothing. `grep` for its counters outside its own module returns
no consumers: it is a default-off diagnostic. The suppression runs on the
necessary condition alone.

## Measured

`CRATONVM_DBG_VERIFY_OOP_MAPS=1`, oracle rewritten to walk the RBP chain and
union each frame's own maps at its own frame base (the previous version built
its mapped set from one method's maps at one frame base, so a nested callee's
correct slots read as unmapped — its counts were an upper bound and unusable).
Hits are recorded as distinct `(method, slot offset)` pairs and only on frames
whose `fully_oop_covered` is `true`.

```text
probe (generational)  DISTINCT never-mapped sites=4  by_class={"operand-spill": 4}
                      all four in one method, at rbp-0x80/0x90/0xa0/0xa8
ntru  (gen / g1)      4 / 2 hits, all class=operand-spill
probe (G1)            0 sites, 24 frames, 336 words
```

**Every hit is in the operand-spill region.** Zero in the three storage classes
the `refresh_moving_young_coverage_for_current_thread` module block predicts as
undescribed (scalar-replacement fields, LICM hoist slots, the full-GPR
safepoint spill area). The gap is not in what the model admits it cannot
describe — it is in the operand stack, which the model claims.

## What this does NOT establish

* **No individual site is proven to be a live oop.** The oracle validates a word
  with `is_object_address`, so a primitive whose bits land on a live object
  header reads as an unmapped oop. Four adjacent slots in one method is a poor
  fit for coincidence, but the mechanism above is established by *reading the
  emitter*, not by these counts.
* **No end-to-end corruption is demonstrated for the generational collector.**
  On every workload measured, true generational (`-XX:+UseGenerationalGC`)
  reports `incomplete=true` for other obligations on every collection, so it
  never takes the suppression:

  ```text
  truegen  precise_only=false  incomplete=true  ybounds=true   8 of 8 collections
  ```

  The two collectors where the proof *did* pass are G1 and ZGC, and
  `collect_roots` no longer lets either take the branch. So the exposure that
  remains is latent: a workload where generational's other obligations all pass
  would take a suppression backed by this bit.

## Relationship to the G1 bug

`bug-g1-evacuates-live-jit-reference-20260819.md` is the same defect observed
from the consumer end: G1 skipped the conservative scan on this proof, published
no pins, and evacuated a live `StringLatin1.newString` reference. That record's
fix stops G1 depending on the bit. This record is the bit itself.

## The repair

Making the bit *sufficient* is the real fix, and it is bounded: have
`emit_oop_map_for_safepoint` refuse to claim a mapped safepoint when any of the
three drops fires. Concretely, track a per-safepoint `lost_an_oop` and exclude
that pc from `mapped_safepoint_pcs` — which flips `fully_oop_covered` to `false`
for the method and returns it to the conservative backstop. That is the
fail-closed direction the same function already takes for a missing paired
spill (`live_frame_hi == 0` = "unknown", scan conservatively).

Wiring `stack_oop_marks_exact` into the predicate is the one-line subset of
this and closes drop 1 alone.
