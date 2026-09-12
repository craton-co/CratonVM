# G1 holds a whole region for one interior array cursor in a compiled frame — 2026-09-12

| | |
|---|---|
| **Status** | **OPEN.** A conservative JIT band word holding an address INSIDE an array — not at its base — is published as a root, fails the collector's object-grid screen, and `pinned_region_set_including_non_object_roots` pins its entire 1 MB region. Nothing downstream can narrow it: the movable/unrewritable partition is keyed on rewriting, and an interior address is not a `PointerMap` key, so no rewriter could ever be named for it. |
| **Cost, measured** | one G1 region per affected pause. On `probes/TvmProbe.java` (`-XX:+UseG1GC --Xmx 2g`) that is the difference between a row reading **3251** and **2227** against a **2928** threshold — i.e. it is the whole of what still fails. |
| **Scope** | G1 only. ZGC resolves an interior root to its base and withholds the PAGE (`ZgcRealHeap::resolve_interior_for_pin`); the generational collector does not relocate while a thread is in JIT. G1 is the collector whose pin granularity is a megabyte. |
| **Found while** | retiring `internal/fixed-bugs/h2-testvaluememory-g1-conservative-jit-roots-RESOLVED-20260912.md`, whose own named causes are all now implemented or refuted. This is what its symptom turned out to be. |

---

## 1. What it looks like

One line, from `CRATONVM_G1_DBG_PINS=1`:

```text
[g1] root is not an object (#32): addr=0x26af58001d0 verdict=Object region=75
     type=Eden base=0x26af5800000 cursor=0x400d0 off=0x1d0 age=0 pinned=false
     reuse_epoch=13 recycled_in_generation=137
     grid=INTERIOR of=0x1a0 delta=0x30 size=0x60 cid=0 kind=Array idx=5
     — pinning region 75 instead of evacuating it.
```

`grid=INTERIOR of=0x1a0 delta=0x30 size=0x60 kind=Array` is the whole finding:
the address is 0x30 bytes into a 0x60-byte array. `verdict=Object` beside it is
the *other* screen's answer — `heap.is_object_address`, the one the band scan
uses, reads plausible header bytes at that address and accepts it. The object
grid, which walks the region, knows better.

## 2. Why no existing narrowing reaches it

Two publishers feed G1's pin set and the second undoes the first.

```text
[g1][MOVPIN] snapshot=7 kept=1 movable_claimed=7 unrew_veto=1
             honour_movable=true coverage_incomplete=false movable_set=7
[g1][PINS]   pin_addrs=7 pin_regions={0, 1, 5, 75} pinned_bytes=2000K
```

`jit_pinned_region_set` applied the movable/unrewritable partition and kept
**one** region — the live humongous `array`, which is not a young-CSet candidate
anyway. The set that actually decides the CSet has **four**, because
`pinned_region_set_including_non_object_roots` walks the full root array
afterwards and re-adds the region of every root that is not the start of a live
object.

That function is right, and its own doc comment is the reason this page has no
easy fix in it:

> Evacuating such a root is unsound and so is skipping it, for the same reason:
> the collector cannot tell a real reference from a long, so it may neither
> rewrite the slot nor drop what it might point at. Pin the region instead.

**And the movable partition cannot be extended to cover it.** A movable claim
says every band sighting of this address is either rewritten by
`remap_one_jit_frame` or dead. `remap_one_jit_frame` rewrites through a
`PointerMap` keyed by object BASE — an interior address is not a key, so the
rewrite half of that claim is unavailable by construction. Honouring a movable
claim for an interior address would be licensing a move nothing fixes up.

## 3. Why region granularity is the cost

G1 frees a region by evacuating everything out of it, so one unmovable address
holds the whole megabyte whatever the collector does around it. On the measured
pause:

| | bytes |
|---|---:|
| the object the interior word is inside | 96 |
| region 75's occupancy, held out of the CSet for it | **1024 KB** |
| the row's `Used memory`, against a 2928 KB threshold | **3251** |

Shrinking `region_size` is not the answer and the arithmetic is in the retiring
page: `G1_TARGET_REGION_COUNT` is held near 2048 on purpose, and thirteen pinned
regions at 128 KiB would still be ~1.6 MB.

## 4. Where the repair goes

**In codegen, not in the collector.** A compiled loop that keeps
`array_base + header + i*stride` in a frame slot across a GC-capable safepoint
is producing a derived pointer with no base recorded beside it. The tree already
knows this shape — `xt_root_scan`'s `helper_window_pin_resolve_enabled` names it
exactly, *"a cursor into a `char[]` or `byte[]`"*, and ZGC grew
`resolve_interior_for_pin` to handle it — but every consumer so far has handled
it by RETAINING more, which on G1 means a megabyte.

The two shapes of fix, in increasing difficulty:

0. **Record the base beside the derived pointer.** The classic (base, derived)
   pair in the oop map, so the collector can move the array, rewrite the base
   slot and re-derive the cursor. This is what the interior word is missing and
   the only repair that makes the region evacuable.
1. **Do not keep a derived pointer live across a safepoint.** Re-materialise the
   cursor from the base after the call. Cheaper to reason about, and it costs an
   `LEA` per safepoint in an array loop.

**What is NOT the fix**, each refuted by measurement on the retiring page or
here: narrowing G1's pin set by the movable partition (it cannot name a rewriter
for an interior address), narrowing the conservative band scan by storage class
(a four-class bisect moved the failure count while moving the total by under
2 %, i.e. it moved nothing), and shrinking `region_size`.

## 5. The floor, so the size of the prize is not in doubt

`CRATONVM_DBG_NO_JIT_ROOT_SCAN=1` — unsound, and the point is the number:

| `probes/TvmProbe.java`, G1, `--Xmx 2g` | rows over threshold | worst row | sum of 40 rows |
|---|---:|---:|---:|
| `dev` (both trees measured) | mean 4.5–4.7 | 3331 | ~92500–93100 |
| + the deopt `SavedRegisters` partition | mean 1.8–1.9 | 3252 | ~80200–80500 |
| **no conservative JIT roots at all** | **0** | **2228** | **57621** |

Every row passes in the floor arm, so the JIT root set is the entire remaining
excess and nothing else about G1 needs to change.

## 6. Repro and instruments

```bash
H2J=~/.m2/repository/com/h2database/h2/2.4.240/h2-2.4.240.jar
javac -cp "$H2J" -d /tmp/p probes/TvmProbe.java
CRATONVM_G1_DBG_PINS=1 cratonvm --java-home "$JDK25" \
    -XX:+UseG1GC --Xmx 2g -c "/tmp/p:$H2J" TvmProbe
```

Then `grep 'root is not an object'` for the `grid=INTERIOR` rows. The warning is
rate-limited to `n <= 8 || n.is_power_of_two()`, so **the number of printed lines
is not the number of occurrences** — read the `(#N)` in the last one.

Run arms ALONE or as concurrent pairs. The failure boundary here is 50–330 KB
wide on a 2928 KB threshold, and this host's load moves a row by a full region.

## 7. A second defect this workload reaches, and a measured association

While measuring the above, the **unmodified** `dev@c0bebbde5` binary SIGSEGV'd
on this workload, in SOLO sequential runs, always at the same faulting RVA and
always with the same decode:

```text
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=…+0x380863
Faulting address decodes as an indexed load: [rax+r11*4]
  rax=0x0000025000000000  r11=0x0000000000000002
Java frames (primordial thread): <none published yet>
```

Its crash handler then hangs rather than exiting — two runs had to be killed by
PID — so an arm that never returns is this, not a slow VM.

**Two batteries, each sequential and alternating so any host drift lands on
both arms, the second run after merging `dev` and rebuilding both:**

| battery | arm | runs | SIGSEGV | rows over threshold | sum of all 40 rows |
|---|---|---:|---:|---|---:|
| `dev@c0bebbde5` | base | 12 | **6** | mean 4.67 | 93136 |
| | + the `SavedRegisters` partition | 12 | **0** | mean 1.83 | 80170 |
| `dev@0a805f3fc` | control | 10 | **4** | mean 4.50 | 92535 |
| | + the partition | 10 | **1** | mean 1.89 | 80457 |

Pooled: **10 crashes in 22 control runs against 1 in 22**, Fisher's exact
p ≈ 0.003. **Stated as an association, not a cause** — these batteries were
built to measure retention, the crash is a side observation in them, and nobody
has read the faulting instruction. Note also what the second battery corrected:
the first read 6-to-0 and would have supported "eliminates", which the second
refutes. It is a reduction.

There IS a mechanism that would explain it, and it is the reason this paragraph
is here rather than in a footnote. `remap_one_frame_register_images` rewrites a
moved reference in every region `register_image_remap_admits` accepts, and it
does so **independently of whether the conservative scan rooted that word**.
Before the partition, the deopt `SavedRegisters` GPR image was in no such
region: a reference there that the band scan's `is_object_address` screen
happened to reject was neither pinned nor rewritten, and the deopt stub then
reconstructs an interpreter frame from it (`FrameValue::RegisterRef` ->
`regs.gpr` -> `Object`) at its pre-move address.

Whoever confirms or refutes that needs a crash-focused battery rather than
these: the same two binaries, `CRATONVM_DBG_JIT_NAMES=1` so the faulting pc is
attributed to a compiled method, and enough runs to separate 10/22 from 1/22
with room to spare.
