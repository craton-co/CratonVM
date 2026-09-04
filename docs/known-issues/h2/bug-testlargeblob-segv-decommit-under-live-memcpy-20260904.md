# `TestLargeBlob` SIGSEGV: a granule decommitted under a live 23.9 MB memcpy

**Status:** open. Root cause narrowed to the reserve/commit store's give-back
racing a heap pointer that outlives a collection; the specific unrooted
reference is not yet identified.

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
  (`vm/src/native/jni.rs`, "GC-correctness (vm-jni-roots #2)"). That path is
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
