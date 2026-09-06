# `TestLargeBlob` SIGSEGV: a granule decommitted under a live 23.9 MB memcpy

**Status:** 🟢 **RESOLVED 2026-09-05.** The root cause is the compacting
slide writing into granules the give-back had returned -- `compact_high_region`
for this class's 23.9 MB blobs -- and it is closed by
`Arena::commit_for_relocation` at both slide call sites (`19854a573`, landed
independently as `ensure_committed_span` in `95e4cacc3`). **There is no
unrooted reference to find**; the "missing root" reading below is retracted in
the Resolution section, along with the operand attribution that produced it.

Verified by A/B on one pinned commit (`7acc0b27c`), two binaries differing only
in the two guard call sites: **guard ON 4/4 PASS, guard OFF 4/4 CRASH**
(Fisher's exact p = 1/70 = 0.014).

**Reproducer** (Azure `vm1`, `/data/cratonvm/apps/h2database-suite-runner`):

```bash
JDK25=/data/toolchain/jdk-25 CRATONVM_BIN=/data/cratonvm/target/release/cratonvm \
  CRATONVM_ZGC_ALLOC_TRIGGER=25 \
  ./run-h2-suite.sh run --category all --start 33 --count 1 --tag lb
```

`--start 33 --count 1` is `org.h2.test.db.TestLargeBlob`. Roughly 5 crashes in
7 runs.

## Why the trigger flag is in the reproducer and not in the bug

`CRATONVM_ZGC_ALLOC_TRIGGER=25` is not the defect. It collects once 25% of
capacity has been allocated since the last cycle, and on this class that is the
difference between **0 collections and 34**:

| configuration | GC cycles | result |
|---|---|---|
| no allocation trigger | 0 | PASS |
| `CRATONVM_ZGC_ALLOC_TRIGGER=25` | 34 | SIGSEGV |

So the flag is a reproducer for something that was always there and that
nothing was exercising — this class never collected at all before. The flag was
briefly a default (an implicit floor under `CRATONVM_ZGC_PAUSE_TARGET_MS`) and
was withdrawn on 2026-09-04 for exactly this; see
`alloc_trigger_percent_for`.

## The proximate cause is the decommit, and that is measured

`CRATONVM_GC_RESERVE=0` disables the reserve/commit store, so free granules are
never handed back to the OS. Same binary, same class, trigger on in both arms:

| arm | result |
|---|---|
| `RESERVE` on (default) | CRASH, CRASH, PASS |
| `RESERVE=0` | PASS, PASS, PASS |

With the earlier runs that is **5 crashes in 7 with the give-back active and 0
in 6 without it**. The decommit does not create the dangling read; it converts
one that used to land on stale-but-mapped bytes into a hard fault. That is not
a reason to turn the give-back off: it is the only thing making a pre-existing
defect visible.

## The register state says the same thing

```
SIGSEGV at pc=0x71c3161a1794, addr=0x71c3069fffc0
rdi=0x71c305ac4990  rsi=0x71c3054235c0  rdx=0x16c8010
fault pc is in libc.so.6 (r-xp 71c316028000-71c3161b0000)
```

A libc `memcpy` of `rdx` = 23,887,888 bytes. The fault address is
`rsi + 22,924,288` — about 96% of the way through, i.e. reading the SOURCE.
And `0x71c306a00000` is exactly 2 MiB-aligned, which is
[`Arena::decommit_span`]'s granule. The source region stops dead on a granule
boundary partway through a copy: those pages were given back to the OS while
the copy was reading them.

## What has been ruled out

* **JNI array access.** `Get<Type>ArrayElements` hands native code a detached
  COPY and mints a global ref for the source as a keep-alive root
  (`../../../vm/src/native/jni.rs`, "GC-correctness (vm-jni-roots #2)"). That path is
  correct and is not this.
* **The relocating slide.** Relocation is opt-in (`CRATONVM_ZGC_RELOCATE`) and
  off in these runs. The slide had its own instance of this fault shape, fixed
  on 2026-09-04 with `Arena::commit_for_relocation`; this is the same family
  reached by a different path.

## Two leads, in order of suspicion

1. **`pin_critical` does not keep anything alive.** `critical_pin_addrs()` has
   exactly one caller — inside `relocate_stw`, feeding the relocation-set
   filter — so a JNI critical pin makes an object IMMOVABLE and nothing else.
   On a non-moving run it is inert: the object can be swept, free-listed and
   decommitted while a native holds a raw pointer into it. That is a real
   defect whether or not it is the one behind this crash, and it is cheap to
   state: a pin taken for a critical section should be a root.
2. **`ByteBuffer.allocateDirect` is arena-backed.**
   `native_heap_bytebuffer_allocate` allocates an ordinary Java `byte[]` and
   wraps it, so a "direct" buffer is a collectible heap object here. Anything
   that takes its address and then blocks — an I/O write, a channel transfer —
   is holding a heap pointer across a safepoint. The crashing stack in the
   sibling FAIL arm was `FileChannelImpl.implWrite` -> `IOUtil.write` ->
   `DirectByteBuffer`.

## The instrument that will settle it, now built

The crash reporter already answered "fault pc is in NO recently freed code
buffer" for executable memory. As of 2026-09-04 it answers the same question
for the heap: `reservation::recent_decommit_covering` keeps a 64-entry ring of
the spans this process handed back to the OS, and the handler prints

```
#  gc_decommits_total=0x…
#  fault addr is inside a RECENTLY DECOMMITTED heap span: base=0x… len=0x… site=free-list-low
#    *** and NOT re-committed since. This IS a use-after-free of heap memory …
```

Two things in that line are the whole point.

**The site.** Every give-back is tagged where the liveness proof was made —
`free-list-low` / `free-list-high`, `low-cursor-retract`,
`high-cursor-retract`, `free-tail-retract`, `unbumped-middle`. A `free-list-*`
hit means the span was swept and free-listed while something still pointed at
it: a MISSING ROOT, and the two leads above are then the place to look. A
retract or middle hit means a cursor passed over live bytes: a SWEEP that
mis-sized the live set. That is exactly the fork the evidence above cannot
resolve, and it is now answered by the crash itself.

**The re-commit flag.** A granule given back can be taken again later, at which
point the address is mapped and a fault there is a different question. Slots
are flagged rather than erased on re-commit, so "we gave this away and took it
back" stays reportable instead of degrading to "no record" — which reads
identically to a wild pointer.

Reproduce with the command at the top and read the new lines.

---

## RESOLUTION 2026-09-05 --- it is the slide's DESTINATION, not a missing root

### The measurement

Same commit `7acc0b27c`, two release binaries built from it, differing only in
whether the two `commit_for_relocation(to - base, size)` call sites are live.
Interleaved, `CLASS_TO=1800` so host load could not manufacture timeouts:

| arm | n | PASS | CRASH |
|---|---|---|---|
| guard **ON** | 4 | 4 | 0 |
| guard **OFF** | 4 | 0 | **4** |

Fisher's exact, one-tailed, p = 1/70 = 0.014. The ON runs took 213-250 s; every
OFF run faulted ~0.25 s into the class, on the first slide after a give-back.
All four OFF crashes carry the same tag from this page's own instrument:

```text
fault addr is inside a RECENTLY DECOMMITTED heap span:
  base=0x767e48800000 len=0x4a00000 site=free-list-high
```

`free-list-**high**` in all four -- the large-object end, which is why this
class's 23.9 MB blobs reach it and why `RMapGcStress` never could.

### Why the baseline crashed even though the tree had the fix

The commit that carries this page already had `commit_for_relocation` in its
tree. Its **reproducer binary did not**: the recipe above pins
`CRATONVM_BIN=/data/cratonvm/target/release/cratonvm`, and that checkout was at
`b011f0dc0`, which predates the guard on its lineage. The 5-in-7 was measured on
a guard-less binary. Anyone re-running this page must check what that path was
built from, or repeat the same mistake.

### Retraction: "reading the SOURCE" was not established

"The register state says the same thing" concludes the fault is a read of the
SOURCE, from `addr == rsi + 22,924,288`, 96% through a 23,887,888-byte copy.
That arithmetic is right and the conclusion does not follow, because the two
ranges OVERLAP:

```text
rdi (dst) = 0x71c305ac4990      rsi (src) = 0x71c3054235c0      rdx = 23,887,888
dst - src = +6,951,888  ->  dst is ABOVE src, and the ranges overlap by 16.9 MB
addr - rsi = 22,923,776  (96.0% through the SOURCE)
addr - rdi = 15,971,888  (66.9% through the DESTINATION)   <-- equally true
```

The faulting address lies inside **both** operand ranges, so it cannot
distinguish them. A slide is `memmove`, and `dst > src` here identifies it as
`compact_high_region`, which packs survivors UP -- so the overlap is expected,
not incidental. The decommitted span explains the destination; nothing needs to
explain a source.

That inference is what sent this page looking for an unrooted reference, and
the search had no target.

### What this defect is NOT

* **Not a missing root.** Nothing was swept while reachable. The span was
  legitimately dead and correctly free-listed; the bug is the WRITER.
* **Not the peer-pinning group.** Killing all three of
  `CRATONVM_XT_PEER_SHADOW_SCAN`, `CRATONVM_XT_PINNED_PEER_DEPTH` and
  `CRATONVM_XT_HELPER_WINDOW_DISCHARGE` leaves this class passing 4/4. Those fix
  the separate OOM (`9920489ec`).
* **Not `CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE`.** It is
  `runtime_var_os(..).is_some()`, i.e. opt-in and default OFF, so it was never
  active in any run on this page.
* **Not the high-end bitmap water mark.** Reverting `high_low_water()` to the
  raw `high_cursor()` leaves this class passing 7/7.

### Same defect as

`bug-box-unbox-intrinsic-segv-under-relocation-20260902` (retired to
`fixed-bugs/` 2026-09-05),
`RMapGcStress` (`rc=139` in the 2026-09-03 suite run) and
`h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`'s SEGV face.
One collector bug, four reporters.

