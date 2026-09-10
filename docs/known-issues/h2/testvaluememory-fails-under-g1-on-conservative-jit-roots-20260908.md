# H2 `TestValueMemory` fails under `-XX:+UseG1GC`, and only under G1

*Found 2026-09-08 while retiring
`docs/internal/fixed-bugs/h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md`.
Measured 2026-09-09: the "where to start" list is now run to the end, two of its
three items are REFUTED by measurement, and the third has a number on it.*

*Updated 2026-09-09 (second pass): item 2 is IMPLEMENTED and Type 0 now passes
on G1. The failure moved to Type 3, which measurement then showed is not this
page's problem at all — see [Type 0 is fixed; Type 3 is a different
bug](#type-0-is-fixed-type-3-is-a-different-bug).*

## Status

**OPEN at Type 3; Type 0 is FIXED.** `org.h2.test.unit.TestValueMemory`,
`-Xmx2g`.

Before (dev `fc70730d5`):

| arm | result | worst row |
|---|---|---|
| CratonVM default (ZGC) | **PASS**, 40/40 types | 2.28x |
| CratonVM `-XX:+UseGenerationalGC` | **PASS**, 40/40 types | 2.28x |
| CratonVM `-XX:+UseG1GC` | **FAIL at Type 0** | **3.30x** |
| HotSpot JDK 25 | PASS | 0.5x |

After precise register oop maps (`claude/jit-reg-oop-maps-20260909`):

| arm | Type 0 | result |
|---|---|---|
| CratonVM default (ZGC) | 2227 | **PASS** |
| CratonVM `-XX:+UseGenerationalGC` | 2227 | **PASS** |
| CratonVM `-XX:+UseG1GC` | **2227** | FAIL at Type 3 (~10000) |

G1's Type 0 went 3224 -> 2227, which is the same number the other two
collectors read, i.e. the G1-specific excess on that row is GONE. All three
arms are now identical there.

The assertion is `used > memory * 3`, so the threshold is 2928 KB. What has
changed since 2026-09-08 is not just the number but what is known about it, and
the first correction is to what this test measures at all.

## The 977 KB floor is a LIVE object, and the predecessor page mis-stated it

`h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md`
closes on this:

> The last 2x — 977 against HotSpot's 488 — is **not** accounted for here. […]
> a 16-byte uniform value cell against HotSpot's compressed object layout is the
> obvious candidate.

The right instinct pointed at the wrong object. Read `testType`:

```java
Object[] array = list.toArray();          // 125 000 slots
IdentityHashMap<Object, Object> map = new IdentityHashMap<>();
for (Object a : array) { map.put(a, a); }
int size = map.size();
map.clear(); map = null; list = null;     // <- `array` is NOT dropped
System.gc(); System.gc();
long used = Utils.getMemoryUsed() - first;
...
    "… length: " + array.length + " size: " + size;   // <- read AFTER `used`
```

`list` and `map` are nulled; **`array` is not, and it is read after the
measurement.** So a 125 000-slot `Object[]` is a live local across both
`System.gc()` calls, and every JVM is obliged to retain it:

| | reference width | array bytes | reads |
|---|---|---:|---:|
| HotSpot, compressed oops | 4 | 500 016 | **488 KB** |
| CratonVM | 8 | 1 000 016 | **977 KB** |

That is the whole of the 0.5x-versus-1.0x gap, and `--nojit` reading 977 on
every collector is not over-retention at all — it is the floor, reached exactly.
**No root-scan work can go below 977 KB on this test**; only compressed
references could. The predecessor's "not accounted for" is discharged, and its
16-byte-value-cell hypothesis is refuted: the values are `ValueNull.INSTANCE`,
one singleton, 125 000 references to it — `size: 1` in the assertion message
says so.

This matters because it fixes the target. The budget is `used <= 2928`, the
floor is 977, so there is 1951 KB of headroom and G1 is 296 KB over it.

## Measured: three regions, five addresses, and what the pins cost

`CRATONVM_G1_DBG_PINS=1` now prints the bill rather than just the region numbers
(`G1Collector::describe_pin_set`, added here). On the pause the assertion reads:

```
[g1][PINS] young pause (parallel): jit_active=true pin_addrs=5 pin_regions={0, 11, 16}
  pinned_bytes=2520K
  [0:Survivor        occ=547K/1024K pins=2 jit 0x…0000010(+0x10,Object,64B) 0x…000da98(+0xda98,Object,16B)]
  [11:HumongousStart occ=976K/1024K pins=1 jit 0x…0b00000(+0x0,Object,1000016B)]
  [16:Eden           occ=997K/1024K pins=2 jit 0x…1000170(+0x170,Object,96B) 0x…1000240(+0x240,Object,56B)]
```

Read it as a ratio and the defect states itself:

| | bytes |
|---|---:|
| objects the five conservative words name | 1 000 248 (**977 KB**) |
| heap those five words hold out of the CSet | **2520 KB** |
| `Used memory` the test then reads | 3224 KB |

**Region 11 is not the problem.** It holds the live `array` above, in its own
humongous region — and a humongous region is not a young-CSet candidate anyway,
so pinning it changes nothing. It is correctly retained on every JVM.

**Regions 0 and 16 are the whole of it.** 1544 KB of Eden and Survivor held out
of the collection set to protect 232 bytes of objects: 64 + 16 in region 0, and
96 + 56 in region 16. That is the region-granularity cost the 2026-09-08 page
predicted, now weighed — G1 pays about 6600x the object size for these pins,
where ZGC's page-granular equivalent costs it 275 KB.

## Item 1 of the old "where to start" is REFUTED

The page proposed narrowing the pin set with `classify_candidate_header_view`,
"which decides whether an address is an object BASE". The census prints that
verdict per pin address, and **all five are `Object`** — exact bases, valid tag
bytes, sane shapes. There is nothing for a base test to reject, because the
conservative scan already screens every candidate through `is_object_address`
before it becomes a root. The narrowing was already implemented, upstream, and
it is why the set is five addresses rather than five hundred.

## A second, better-founded narrowing is also refuted — and found a real defect on the way

The generational young sweep does not pin every conservative root. It pins by a
partition (`gen_heap.rs`, `honour_movable`):

```rust
let movable = honour_movable
    && crate::gc_quiescence::is_movable_jit_root(a)
    && !crate::gc_quiescence::is_unrewritable_jit_root(a);
if is_y(a) && !movable { pin_base_of(a, &mut pinned); }
```

"Movable" is a claim that every word naming this object sits in a rewritable
channel; "unrewritable" is the veto the band scan publishes for a word in a
region `band_slot_is_verifiable` refuses to inspect. **G1 consumes neither** —
its pin set is `roots[jit_scan_start..]`, every address the conservative scan
produced. Applying the generational predicate looked like the narrowing this
page wanted.

It is not, and `[jitpins]` (added here) says so in one line:

```
[jitpins] words=15 distinct=5 unrew_set=4 movable_set=0
  0x…0000010(unrew=0,movable=0) 0x…000da98(unrew=1,movable=0)
  0x…0b00000(unrew=1,movable=0) 0x…1000170(unrew=1,movable=0) 0x…1000240(unrew=1,movable=0)
```

**Four of the five carry the veto, and the fifth shares region 0 with one that
does.** The narrowed set pins the same three regions. Refuted.

### The defect it did find: `movable_set=0` is an ordering bug

`movable_set` is empty at the point G1 publishes its pins, on every cycle, and
not because nothing is movable. `collect_roots` clears the movable set at its
top, publishes G1's pin set at step 14, and only reaches the SHADOW-STACK scan —
the sole producer of `add_movable_jit_root` — at step 15. **G1's pin set is
computed against a set that is empty by construction.**

It is not what fails this test (the veto covers four of five either way) and it
is not a correctness hazard: an empty movable set means "pin it", which is the
safe direction. It is left standing rather than fixed here for one reason — the
repair narrows a pin set, that is the direction in which a mistake dangles, and
this workload cannot exercise it. Whoever fixes it needs a workload where
`movable_set` is non-empty AND the pins differ, and `[jitpins]` is how they will
know they have one.

## What the five words actually are

`[bandword]` (added here) names the storage class each unverifiable word sits in
and, for the blind spill, the register:

```
0x…000da98 off=152/168/176/184 region=operand-spill              (above live_hi)
0x…000da98 off=408             region=outgoing-args-or-deopt-regs
0x…0b00000 off=600             region=safepoint-gpr-spill-image  reg=r15
0x…1000170 off=544             region=safepoint-gpr-spill-image  reg=r8
0x…1000170 off=640             region=outgoing-args-or-deopt-regs
0x…1000240 off=160/192         region=operand-spill              (above live_hi)
0x…1000240 off=552             region=outgoing-args-or-deopt-regs
```

Not one is a `callee-saved-gpr-image`, which is what the 2026-09-08 page's
"conservative JIT-frame roots" framing implied and what
`gc_quiescence`'s unrewritable-root module comment leads with. Three classes,
and they are three different problems:

* **`safepoint-gpr-spill-image`** — the write-only blind GPR spill
  (`emit_pre_safepoint_spill`). `r15` holds `array`, which is a **live** local,
  so that word is correct and must stay. `r8` holds a leftover: it is an ABI
  argument register, kept by `oop_capable_spill_regs`' source 2 at every
  safepoint whether or not this one stages arguments.
* **`operand-spill` above `live_frame_hi`** — the only class the VM already
  CLAIMS is dead (`OopMapEntry::live_frame_hi`: "their contents are dead — the
  spill cursor reclaims by moving, it does not clear").
* **`outgoing-args-or-deopt-regs`** — the outgoing stack-argument reserve, which
  `SUB RSP, frame_size` never initialises, so it holds whatever a previous,
  deeper frame left below the old stack pointer.

The narrowed blind spill's known stale-slot residue is NOT what holds these
objects, and the A/B says so: `CRATONVM_JIT_SPILL_NARROW=0`,
`CRATONVM_JIT_SPILL_ARGS_PUBLISHED=0`, `CRATONVM_JIT_CALL_SPILL_ELISION=0` and
`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`, singly and together, all read **3224**.
The registers genuinely hold these addresses at the safepoint.

## Item 2 is the fix, and it is now priced

`CRATONVM_JIT_BAND_SKIP=<class>[,…]` (added here — **default unset, unsafe, a
measurement lever and not a fix**) drops named storage classes from the
conservative band scan. It answers the question that was blocking the decision:
what would precise compiled-frame roots be worth?

| band scan | `Used memory` | verdict |
|---|---:|---|
| baseline | 3224 | FAIL, 3.30x |
| skip `operand-spill` | 3225 | FAIL |
| skip `outgoing-args-or-deopt-regs` | 3224 | FAIL |
| skip `safepoint-gpr-spill-image` | 3224 | FAIL |
| skip `operand-spill` + `outgoing-args-or-deopt-regs` | 3224 | FAIL |
| **skip all three** | **2228** | **PASS, 2.28x** |

Two things to take from that table, and the second is why no cheaper fix exists.

**The ceiling is exactly ZGC's number.** 2228, to the kilobyte, on the arm whose
pins cost 1544 KB more. With the false roots gone, G1's region granularity costs
nothing extra here, because what remains is the live `array` in its own
humongous region plus the genuine live set. So the region-granular pin is not
independently worth fixing on this workload: remove the false roots and it stops
mattering. That is the measurement the 2026-09-08 page asked for in its item 0,
and it inverts that page's own ordering — it put "narrow what a pinned region
costs" ahead of precise roots, and the pinned region costs nothing once the
roots are precise.

**No subset buys anything.** Every pinned object is named from more than one
class, so any two-of-three leaves all three regions pinned and the number
unmoved. There is no incremental landing here; a partial precise root set gets
zero.

## Type 0 is fixed; Type 3 is a different bug

Item 2 was implemented (`OopMapEntry::reg_oop_mask`, plus the two companion
narrowings item 0 and item 1 called for). Type 0 on G1 went **3224 -> 2227**,
identical to ZGC and generational on the same row. That is this page's headline
number, and it is discharged.

The suite still fails, now at Type 3, and the rest of this section is the
measurement that says **Type 3 does not belong to this page**.

### Skipping EVERY conservative band class changes nothing at Type 3

`CRATONVM_JIT_BAND_SKIP` exists to price exactly this. Set to all ten of
`region_name`'s classes it is the absolute ceiling of the conservative-root
line of work — no compiled-frame word is a marking root at all:

| `CRATONVM_JIT_BAND_SKIP` | Type 3, three runs |
|---|---|
| unset | 10976, 10967, 12998 |
| all ten classes | 11979, 11986, 11987 |

Indistinguishable, and if anything higher with the skip. **The remaining Type 3
excess is not in the band scan**, so no amount of further root-set precision —
including the deopt `SavedRegisters` block, which the previous revision of this
page named as the next step — can reach it. That plan is retired.

The JIT is still implicated: `--nojit` reads **793** on G1, twice, exactly. So
the cost arrives through the JIT by some channel other than the band scan.

### The channel is precise oop-map roots, pinned a megabyte at a time

Under the full band skip, G1 still pins 12 regions:

```
pin_addrs=30 pin_regions={7,8,9,10,14,15,16,17,21,23,24,68} pinned_bytes=9708K
```

Thirty pin addresses survive a total band skip because they were never band
words. They are the frames' **precise** roots — `frame_slot_offsets` from the
oop maps, i.e. references the compiler positively asserts are live. They are
correct roots. The cost is that G1 excludes a pinned region from the collection
set WHOLESALE, so ~30 live references scattered across 12 regions cost ~10 MB at
`-Xmx2g`, against a 2928 KB threshold.

That is a G1 pinning-granularity problem, not a root-precision problem, and it
is why the same 30 references cost ZGC 1205 and the generational collector 1149
on the identical row.

### The movable partition is right, inert, and starved

Precise map roots are exactly the ones `remap_active_jit_frames` rewrites after
a move, so they ought not to be pinned at all. `gen_heap` already acts on that
distinction; `G1Collector::jit_pinned_region_set` did not, and now can, behind
`CRATONVM_GC_G1_MOVABLE_PINS=1` (**default off**). It is off because measuring
it showed the filter is starved, not wrong:

```
[g1][MOVPIN] snapshot=38 kept=38 movable_claimed=2 unrew_veto=9
             honour_movable=false coverage_incomplete=true movable_set=2
```

Two independent blockers. `coverage_incomplete=true` disables the whole-cycle
proof outright. And even forced on, only **2 of 38** addresses are claimed
movable, because `add_movable_jit_root` is reached only from the shadow-stack
scan and only when it is not publishing pinned — the 36 precise map roots
publish themselves to nothing.

So the next person's job is not the filter. It is **teaching the precise oop-map
roots to publish themselves movable**, at which point the filter is already
here and already correct. Until then it stays off, because turning on a live GC
behaviour change worth 2 pins in 38 is a risk bought for nothing.

### Corrected: the "ordering defect" was the diagnostic, not the collector

The previous revision closed with *"the ordering defect means the partition is
not even computed at the point G1 reads it"*. That is wrong and this page
should not have said it. The `[jitpins]` line prints at step 14 and the
shadow-stack scan runs at 14b, so `movable_set=0` on that line is an artefact of
**where the diagnostic prints**. By the time G1 calls `jit_pinned_region_set`
the set is populated — `movable_set=2`, as the `MOVPIN` census above shows from
inside G1's own read. The real defect was simpler and worse: G1 applied no
movable filter at all.

The rest of that paragraph — do not narrow G1's pin set on the strength of this
page — still stands, now for a measured reason rather than a mistaken one.

## What to do, and what not to

> **DONE 2026-09-09.** All three parts below are implemented on
> `claude/jit-reg-oop-maps-20260909` and jointly take Type 0 from 3224 to 2227.
> Each is separately switchable (`CRATONVM_GC_DEAD_SPILL_ROOTS`,
> `CRATONVM_GC_OUTGOING_ARG_ROOTS`, `CRATONVM_GC_REG_OOP_MAPS`) and all three
> are load-bearing: any one alone leaves Type 0 at 3224. Item 1 was NOT solved
> by the prologue-zeroing this list proposed — the safepoint's own
> `pending_staged_args_unmapped` already proves the reserve holds nothing of
> this frame's, so a `Some` register mask carries the proof and the reserve can
> simply be skipped. The list is kept as written for the record.

**The fix is per-safepoint liveness for compiled-frame words** — the compiled
analogue of `runtime::local_liveness::live_locals_mask`, which is exactly why
`--nojit` reads 977 on every arm. Priced above at 3.30x → 2.28x. Its three
parts, in increasing difficulty:

0. **`operand-spill` above `live_frame_hi`.** The only one with a written
   deadness argument already in the tree, and one already trusted — it is the
   claim `band_slot_is_verifiable` spends to excuse those words from
   shadow-stack publication. Dropping them from the ROOT set is a strictly
   stronger use of the same claim (a wrong answer frees, rather than fails to
   rewrite), so it needs its own verification and not an appeal to the existing
   use.
1. **`outgoing-args-or-deopt-regs`.** Do NOT simply stop scanning it: a
   reference passed as the seventh-or-later stack argument lives there, and the
   callee reads it from the CALLER's band, which is the only scanner of those
   words. The sound repair is to zero the reserve in the prologue — that removes
   only values predating the frame, which is the stale-below-SP class this
   census found, and keeps every argument the frame actually stages.
2. **`safepoint-gpr-spill-image`.** Needs register-level oop maps.
   `OopMapEntry` carries `frame_slot_offsets` and says nothing about registers,
   so this is a codegen change, and it is the project the 2026-09-08 page named.
   `r15`/`array` is the reminder that it must be a LIVENESS answer rather than a
   blanket exclusion: one of the two spill-image words on this workload is a
   genuinely live local, and dropping it would be a use-after-free.

**Do not** raise the threshold, exclude the class, or shrink `region_size`. The
test measures something real, two of three arms pass it, and the region count is
held near 2048 on purpose (`G1_TARGET_REGION_COUNT`).

**Do not** narrow G1's pin set by the movable/unrewritable partition on the
strength of this page — see [the movable partition is right, inert, and
starved](#the-movable-partition-is-right-inert-and-starved) for the measurement,
and note that this paragraph's original reason (an "ordering defect") was itself
wrong and is corrected there.

## Reproducer

```bash
H2=<h2 checkout>
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"

cratonvm --java-home "$JDK25" -XX:+UseG1GC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory        # rc=1 at Type 0, ~1 s, Used memory: 3224
cratonvm --java-home "$JDK25" --nojit -XX:+UseG1GC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory        # Type 0 reads 977 — the live `array`
```

and the three censuses this page is built on, each free when unset:

```bash
CRATONVM_G1_DBG_PINS=1       # per pinned region: type, occupancy, pin count,
                             # provenance (jit/tlab/nonobj) and object sizes
CRATONVM_DBG_JIT_ROOTSCAN=1  # [jitpins]  distinct pin addresses with unrew/movable
                             # [bandword] each unverifiable word's storage class + register
CRATONVM_JIT_BAND_SKIP=operand-spill,outgoing-args-or-deopt-regs,safepoint-gpr-spill-image
                             # the ceiling: 2228. UNSAFE — measurement only.
```

`probes/RealDrop.java` still isolates the shape without H2, and its rows are
unchanged from 2026-09-08.
