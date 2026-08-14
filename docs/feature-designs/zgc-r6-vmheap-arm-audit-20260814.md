# R6 — every `VmHeap::Zgc` arm, audited against a collector that now moves

**Written 2026-08-14.** `zgc-production-implementation-plan.md` filed this as
risk **R6**:

> **The `VmHeap::Zgc` neutral arms are silent.** 58 arms plus a macro; the
> affirmative ones (`true`, `(0,0)`) assert facts that are only true for a
> non-moving collector, and none of them will fail loudly when they become
> wrong.

Compaction went default-on for ZGC on 2026-08-13, so "when they become wrong"
is now. This is that audit, and it found **three live defects**, two of which
corrupt memory.

## Method

74 `VmHeap::Zgc` arms. 46 delegate to the heap (`dispatch!` or `VmHeap::Zgc(h)`)
and are audited by the heap method they call. The 28 that **ignore** the heap —
`VmHeap::Zgc(_) => <constant>` — are the ones R6 is about, because a constant
cannot notice that the collector changed underneath it. Each was classified by
asking one question: **does this answer stay true when an object moves?**

---

## The three defects

### D1 — `pin_critical_region` pinned nothing *(memory corruption)*

```rust
VmHeap::Zgc(_) => Vec::new(),
```

The doc above it explains why that was fine: *"the copy-back staleness is
reachable in practice only under G1's aggressive young/mixed evacuation."*
That reasoning expired the day this collector started evacuating.

The hazard is **not** the native pointer — this VM hands native code a *copy* of
the array. It is the copy-back at `ReleasePrimitiveArrayCritical`, which
re-resolves the Get-time address. Move the array and that copy-back writes a
whole array's worth of bytes over whatever object now occupies the old address.
The JNI site's own comment names the outcome: *"data loss / write to a recycled
object."*

**Fixed** by giving `ZgcRealHeap` a refcounted `critical_pins` set and dropping
any page holding a pinned object out of the relocation set — page granularity,
mirroring what G1 already does with `pin_region_for_addr`. Object granularity
would need the slide's placement probe to route around individual survivors
inside a page it is otherwise compacting, and every extra rule in that probe is
another chance to overwrite something live.

Tests: `a_critically_pinned_object_is_never_relocated` (verified red with the
filter removed) and `critical_pins_are_refcounted_so_a_nested_release_does_not_unpin`.

### D2 — the reference-processing guard tested the pre-move address *(silent loss)*

Not a `VmHeap` arm but found through one — `pre_gc_addr_did_not_survive`'s ZGC
arm is `!self.is_addr_live(addr)`, which correctly answers "that address is not
safe to write through" for a relocated object's *old* address. The caller then
did this:

```rust
if is_stale_young(ref_addr) { continue; }             // tests the OLD address
let actual_addr = pointer_map.get(&ref_addr)...;      // writes through the NEW one
```

**The guard and the write disagreed about which object they meant.** Under a
non-moving collector they are the same address and it never mattered. From
2026-08-13, every `Reference` the slide moved was judged dead here and silently
never cleared or enqueued: no `WeakReference` delivery, no `Cleaner` action —
which on netty is how a direct `ByteBuf`'s native memory stops being freed.

**Fixed** by testing the relocated address in both the cleared and the enqueue
loop. A no-op wherever the pointer map is empty, i.e. every non-moving cycle on
every backend.

### D3 — `prune_dead` received pre-slide addresses *(memory corruption)*

Also not an arm, and the worst of the three. `MonitorCleanup::prune_dead`
documents its input as EXACT — *"this address was a live allocation base before
this collection and its memory is now freed, so no thread can read its mark
word"* — and on that licence calls `Monitor::release_mark_ref`, dropping the
strong reference the object's own mark word owns.

Compaction breaks that precondition in the commonest way there is: survivors
slide **down** into the space vacated by dead objects. Measured on a
four-page fixture: **169 live object bases** were handed to `prune_dead` in a
single collection. Each is a released mark-word reference on a live object's
monitor — a use-after-free reachable from any `synchronized` block, on an object
chosen by wherever the slide happened to land.

**First fix was wrong, and the suite caught it the same day.** Re-screening
`dead` against the post-slide registry stopped the collector freeing a live
object's monitor — and left the *dead* object's entry sitting at that address
for the survivor to inherit, because `remap_after_gc` RETAINS an entry absent
from the pointer map. `io.netty.util.ResourceLeakDetectorTest` went from FAIL to
CRASH on it. The screen fixed one bug and created its mirror image.

**The real fix is ORDER: prune the dead first, then remap the survivors.** With
the prune running first, every address in `dead` really is a freed base — no
survivor has been re-keyed onto one yet — which is precisely the precondition
`prune_dead` documents and which licenses its `release_mark_ref`. The remap then
moves each survivor into a slot nobody else claims.

Test: `a_survivor_keeps_its_own_monitor_and_never_inherits_a_dead_objects`. It
models the real table (drain-and-reinsert, retain-if-absent) and asserts the end
state rather than the call order. Verified red against **both** wrong
orderings, which is what separates them:

| ordering | outcome |
|---|---|
| remap → prune (original) | **169 survivors lost their monitor** |
| filtered remap → prune (the bad fix) | **83 survivors inherited another object's monitor** |
| prune → remap (correct) | 0 and 0 |

**The lesson worth keeping** is not "screen the list". It is that two operations
keyed by the same address space cannot be reasoned about independently once the
collector moves objects between them: `remap` and `prune` were each correct
alone and collided only because compaction made a dead base and a live base the
same number. Fixing the one that noticed first produced a bug in the other.

---

## The other 25 arms

| arm | answer | verdict |
|---|---|---|
| `try_alloc_object_old`, `try_alloc_objects_old_batch`, `old_gen_needs_gc`, `old_gen_info`, `old_gen_lock`, `collect_young_to_old_roots`, `young_gen_stats`, `is_in_young_addr`, `young_inactive_semispace_range`, `dbg_first_young_small_ref` | `None` / `(0,0)` / `false` / empty | **Sound.** Generational concepts. ZGC has one space; the absence is real, not an approximation, and stays true under a moving ZGC. |
| `g1_should_start_marking`, `g1_is_marking_active`, `g1_concurrent_mark_step`, `jit_card_table_info` | `false` / `true` / `None` | **Sound.** G1-specific. |
| `refill_tlab` | `None` | **Sound, and deliberately misleading if read alone** — ZGC has TLABs, just not through this door. Documented at the arm. |
| `set_jit_tlab_skip_regions`, `clear_jit_tlab_skip_regions` | `{}` | **Sound today**, gated by `supports_jit_tlab_skip` which is false for ZGC, so no skip region is ever published. Would become wrong if that gate flipped. |
| `young_bump_headroom`, `young_has_free_block`, `try_alloc_young_probe` | `!needs_gc()` | **Sound.** Allocation heuristics; a wrong answer costs a spill decision, never correctness. |
| `reclaimed_hole_at` | `None` | **Sound.** Diagnostic. |
| `watched_pre_gc_addr_survived` | map-then-`is_addr_live` | **Sound under compaction** — checks the pointer map first, so a moved survivor answers `true`. This is the sibling D2 should have been copied from. |
| `metadata_pin_deferrable`, `mirror_pin_deferrable` | `true` | **Sound, and worth re-reading if generational ZGC lands.** "Deferrable" means the pin can wait for the next cycle; that holds while every ZGC cycle is whole-heap. A young-only cycle (plan item G1) makes deferral a way to miss a pin, exactly as the generational arms' comments describe. |
| `flush_thread_satb` | `{}` | **Sound today, wrong the day concurrent marking is armed.** ZGC now has a SATB ingress (`satb_pre_barrier`), but `mark_active` is never set in production, so there is nothing buffered to flush. Plan item C1 must revisit this arm; noted at C4. |

---

## What R6 was right about, and what it was not

**Right:** the arms are silent. Every one of the three defects above was a
constant or an ordering that stayed syntactically valid and stopped being true,
and not one of them failed loudly. D1 and D3 corrupt memory; D2 loses work.
None produced a diagnostic naming the collector.

**Not quite right:** the count. R6 says "58 arms plus a macro" and implies the
risk is spread across them. It is not — 46 delegate and are the heap's problem,
not the enum's, and of the 28 constants, 25 are sound for a reason that has
nothing to do with motion (they describe generations and regions that do not
exist here). **The risk concentrated in the three places where an arm, or its
caller, encoded an ADDRESS assumption**: pinning, survival, and death. That is
the shape to look for the next time this collector gains a capability — not
"which arms return a constant" but "which arms answer a question about an
address".
