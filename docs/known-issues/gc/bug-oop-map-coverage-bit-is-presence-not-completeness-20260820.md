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

## The frame has TWO coverage vocabularies, and the bit describes only one

The three drops above are real, and closing them was tried — every one now sets
`map_incomplete` and withholds the safepoint from `mapped_safepoint_pcs`
(`30370b165`). **It changed nothing on the measured workload**, and that result
is what identifies the actual mechanism:

```text
probe-fixed          DISTINCT sites=4 {"operand-spill": 4}  while_covered=20
probe-presenceonly   DISTINCT sites=4 {"operand-spill": 4}  while_covered=20
```

Byte-identical arms means `map_incomplete` never fired: nothing was *dropped*.
Those oops were never in the precise vocabulary to begin with.

`emit_pre_safepoint_spill` says what the other vocabulary is. The conservative
bound it publishes:

> Includes the **staged invoke-argument buffer**, which sits above the operand
> stack in the same spill reserve and is live for the duration of the call.

and, twenty lines further down, the blind spill of every used callee-saved GPR:

> an oop can also live in a callee-saved register as an operand-stack temporary
> that survives the call, or via a value the per-slot oop tracker fails to tag

Both are deliberate conservative coverage for oops the precise map does not
name. Staged invoke arguments have already been popped off `self.stack` by the
time the map is built, so no map can name them; they sit in the operand-spill
reserve and are live across the call. That is exactly the measured signature —
four adjacent slots in the operand-spill region, named by no map at any
safepoint of the method (`wrong_map=0`), on a frame asserting full coverage.

So the defect is not that the map loses entries. It is that **the frame is kept
safe by two mechanisms and `fully_oop_covered` describes only one of them**, while
being spent to suppress the other.

## The repair

`fully_oop_covered` must additionally require that nothing relies on the
conservative vocabulary at any safepoint of the method — i.e. no staged
invoke-argument buffer live across a call, and no blind-spilled callee-saved
GPR carrying an untagged oop. A method that needs either cannot claim precise
coverage.

The fail-closed drop accounting in `30370b165` stays: those drops are genuine
unsoundness whenever they fire, it is the correct direction, and it is measured
to cost nothing here (`map_incomplete` never fired on any workload run). It is
hardening, not the fix for the sites above.

## Status of the suppression

`precise_only_true` counted per run, with all fixes in:

```text
probe    G1 / generational      0    (never takes the suppression)
ntru     G1                     0
ntru     -XX:+UseGenerationalGC 1    <- first observed instance; test PASSED
ntru     default (ZGC)          0
```

The single generational instance is the first time any run here has reached
"verifier ran, verifier passed, collector moved". It did not corrupt, which is
consistent with the mechanism above being latent rather than always fatal — the
suppressed pause has to coincide with a staged-argument oop that actually moves.
