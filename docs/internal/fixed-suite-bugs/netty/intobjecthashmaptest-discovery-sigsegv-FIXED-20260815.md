# `IntObjectHashMapTest` SIGSEGVs before its first test — FIXED

**Status:** FIXED (2026-08-15). `io.netty.util.collection.IntObjectHashMapTest`
runs **12/12 clean, 35/35 tests, under `-XX:+UseZGC`**, and 28/28 on the `dev`
tip this page was re-measured against. On the binary the open page was filed
from it is **8/10 bad** (4 × SIGSEGV, 4 × hang).

The page asked for four things it had not done — an `hs_err` read, a `--nojit`
arm, a collector comparison, and a narrowing of which test faults. The answer
came from none of them: **symbolising the fault pc** named the subsystem in one
step, and the collector comparison the page wanted turned out to be the whole
story.

---

## What it actually was: `6b21a496e`'s D3, which this page predates

The fault pc symbolises, on the binary the page was filed from:

```text
0x59c7b6fc8922 -> parking_lot::condvar::Condvar::notify_one_slow
                  <- cratonvm_vm::threading::monitor::Monitor::exit
```

A `Monitor` whose memory had been released while a thread still held it — not a
JIT defect at all, which is why "fault pc is in NO live registered code buffer"
was the correct and unhelpful reading of the report. `vm/src/threading/` is
**byte-identical** between the page's commit and the `dev` tip, so nothing about
`Monitor` changed; what changed is who was freeing its mark-word reference.

That is D3 of `6b21a496e` ("R6 audit"), in its own words:

> `MonitorCleanup::prune_dead` received PRE-SLIDE addresses. Compaction breaks
> [its exactness precondition] in the commonest way possible: survivors slide
> DOWN into space vacated by dead objects. Measured on a four-page fixture, 169
> LIVE object bases reached `prune_dead` in one collection, each a released
> mark-word reference on a live object's monitor — **a use-after-free reachable
> from any synchronized block.**

`Monitor::exit` is a synchronized block's exit. The hang the page recorded ("one
run hung instead of crashing, 240 s cap, no output") is the same defect's other
face: a freed monitor whose `notify` is lost rather than faulting.

## Measured — one class, one flag varied

Azure host 2, `apps/netty-suite-runner`, one class per process, `--Xmx 1500m`.

| binary | commit | ZGC runs | SIGSEGV | hang | clean |
| --- | --- | ---: | ---: | ---: | ---: |
| the page's | `c017029b3` | 10 | 4 | 4 | 2 |
| the fix's parent | `a3e5a0405` | 8 | 4 | 0 | 4 |
| **the R6 audit** | **`6b21a496e`** | **10** | **0** | **0** | **10** |
| `dev` | `fd46ad2fd` | 28 | 0 | 0 | 28 |
| `dev` + `CRATONVM_DBG_GC_STRESS=4194304` | `fd46ad2fd` | 10 | 0 | 0 | 10 |

Every crashing run reproduced the page's signature **bit for bit** —
`r10=0xee5fffebdfffff00`, `rbp=0x20042496501` — on a different binary, at a
different load address, three weeks later. That constant is what identifies the
runs as one bug rather than a family.

### Compaction is the trigger, and that is the second measurement

| binary | arm | runs | bad |
| --- | --- | ---: | ---: |
| `c017029b3` | `-XX:+UseZGC` (compaction default-on) | 10 | **8** |
| `c017029b3` | `-XX:+UseZGC` + `CRATONVM_ZGC_RELOCATE=0` | 5 | **0** |

So this class belongs to the cluster `zgc-specific-sigsegv-cluster-20260814.md`
describes, and the page's own "attributed to `dev` rather than to that branch"
reasoning was right.

### What this page got wrong, and it is worth naming

> The rate moved from 1-in-4 to 2-in-3 within an hour on the same binary and the
> same quiet box, so treat "it passed" as no evidence.

Correct, and the reason is not randomness: **the rate tracks box load.** The
crashing runs here clustered exactly when a build and three other arms were
running; the two clean runs on `c017029b3` are its last two, after everything
else had finished. A "quiet box" reading and a "loaded box" reading of this
class are different experiments.

---

## The second defect this page turned up, and who actually fixed it

Re-running the page against `dev` says "fixed", and stopping there would have
missed that the SAME FAMILY was still live. Chasing it produced a Java-level
witness and a wrong diagnosis; the right one landed on `dev` from another
session while this branch was building. Both halves are recorded because the
witness is now this tree's only Java-level regression test for that fix.

### The witness: one object in 200 000 with a null `final` field

`probes/UnmodifiableListIteratorJitProbe` — a probe that already existed, for an
unrelated Spring Boot DevTools failure — died with a bare `NullPointerException`
on `--real-jdk` and passed under `--nojit`. Narrowed, it is
`probes/JitFrameRootRelocationProbe`:

```text
iterations     = 200000
nullFields     = 1          <- delegates == null, on ONE Holder
first bad at   = 10191      <- deterministic, five runs for five
```

`Holder`'s constructor is `this.delegates = Collections.unmodifiableList(copy)`.
Its `this` lived only in a compiled `<init>` frame; ZGC's page slide moved the
object; the frame slot was not — and cannot be — rewritten; and the `putfield`
landed in the vacated span. `main` kept the relocated copy, whose field was
therefore never written. One object in 200 000, silently, with no exception
anywhere near the defect.

Six levers, each of which alone takes it to zero, and one that multiplies it:

| lever | nulls |
| --- | ---: |
| (none) | 1 |
| `CRATONVM_DBG_GC_STRESS=1048576` | **67** |
| `CRATONVM_JIT_OSR=0` | 0 |
| `CRATONVM_JIT_DENY=…$Holder.<init>` | 0 |
| `-XX:+UseG1GC` / `-XX:+UseSerialGC` | 0 |
| `CRATONVM_ZGC_RELOCATE=0` (even under GC stress) | 0 |

### The wrong fix, and why it was wrong

The obvious reading — G1 zero, ZGC one — is that ZGC is missing G1's
conservative-JIT-root pin. It is, and that is not the defect.

`gc_quiescence::pinned_jit_roots_snapshot()` is two-sided: a CONSUMER (a moving
collector dropping those pages from what it is about to relocate) and a PRODUCER
(the VM's root deposits filling the registry). `caf25c3d1` gave ZGC the
consumer; all four producers — `roots.rs`, `update_root_snapshot`, the
blocked-thread deposit in `vm_exec.rs`, and `pin_frozen_peer_roots_for_g1` —
were still gated on `shared.mem.heap.is_g1()`. So the snapshot was empty on
every ZGC cycle and the consumer dropped nothing. **That is still true**, and it
is worth knowing: a capability that is false everywhere reads as an absence
rather than as a bug, and `zgc.rs`'s own
`a_conservative_jit_root_pins_its_page_against_relocation` calls
`add_pinned_jit_root` itself, so it passes with every producer deleted.

Wiring the producers up DOES fix this probe. It was measured: the witness goes
to zero, `UnmodifiableListIteratorJitProbe` passes, `IntObjectHashMapTest` stays
12/12, and `CRATONVM_DBG_ZGC_PINS` showed the pin withholding only 27 of 748
pages (under 4%), so the obvious objection — that page-granular pinning trades
corruption for `OutOfMemoryError` — does not hold either.

**It is still the wrong fix, and `3c0fd9c01` says why in one sentence:**

> `pinned_jit_roots_snapshot()` […] holds what a CONSERVATIVE STACK scan
> recovered, so a pointer that never left a register is absent, its page is not
> withheld, and the object slides.

A pin over the conservative scan protects exactly the pointers the scan can see.
`3c0fd9c01` instead has ZGC read `gc_quiescence::is_active()` — the flag
`gen_heap` (9 sites) and `g1` (6 sites) already use — and decline to relocate at
all while any compiled frame is live. That is a strict superset: it needs no
scan, so a register-only pointer is covered too. The pin branch was therefore
**dropped from this work rather than landed**, because two overlapping
mechanisms for one invariant is how the next reader ends up trusting the weaker
one.

`probes/JitFrameRootRelocationProbe` is kept and is the durable part: it passes
on `3c0fd9c01` and fails deterministically on its parent, and it was the only
Java-level reproduction of that defect.

### What is left latent, for whoever revisits this

`3c0fd9c01` records its own caveat — *"a JIT-busy process compacts less often,
and on this collector compaction is also defragmentation […] Someone should
measure the fragmentation impact on a JIT-heavy workload before this is
considered settled."*

If that refusal is ever relaxed, the conservative-root pin becomes ZGC's
remaining protection — and **it has no producer**. The four sites listed above
are still `is_g1()`. `vm/tests/jit_pin_producer_witness.rs` pins that coupling
so the relaxation cannot happen silently.

## Regression evidence

Every probe diffed against HotSpot JDK 25, both arms, on this branch **and on a
binary built from pristine `origin/dev`** — the control is what makes the
"IMPROVED" rows a measurement:

```text
                                       arm         fix   dev
JdkOnlyCollectionViewProbe             both          0     0   same
ViewClassProbe                         both          0     0   same
SnapshotIteratorShapeProbe             both          0     0   same
StrictIterPrimitivesProbe              both          0     0   same
MapViewBehaviourProbe                  both          0     0   same
ListOutOfBoundsProbe                   both          0     0   same
SubListBehaviourProbe                  3 arms        0     0   same (90/90)
UnmodifiableListIteratorJitProbe       both          0     0   same (dev fixed it)
ImmutableCollectionsDifferentialProbe  --real-jdk    0    16   IMPROVED
ListItrInterfaceProbe                  both          8     8   same
```

`cargo test -p cratonvm-native-collections --all-targets`: one pre-existing
failure, `treemap_for_each_reads_forwarded_action_and_pairs`
(`ClassCastException: java.lang.Integer cannot be cast to java.lang.Comparable`),
**reproduced with `native-collections/src/lib.rs` reverted to `origin/dev`** —
not this branch's. `cargo check --workspace --all-targets` clean.

## Repro

```bash
# the crash, on the binary this page was filed from
cd apps/netty-suite-runner
for i in 1 2 3 4 5; do
  cratonvm --java-home <jdk25> --Xmx 1500m @common.args -XX:+UseZGC \
    -Dcraton.batch=1 CratonRunner io.netty.util.collection.IntObjectHashMapTest \
    > /tmp/iohm-$i.out 2>&1
  echo "run $i rc=$?"        # 139 = SIGSEGV, 124 = hang, 0 = pass
done
addr2line -f -C -e <binary> $((<fault pc> - <load base>))   # -> Monitor::exit

# the relocation defect, on any binary predating 3c0fd9c01
javac -d /tmp/p probes/JitFrameRootRelocationProbe.java
cratonvm --real-jdk --java-home <jdk25> -cp /tmp/p JitFrameRootRelocationProbe
#   nullFields = 1   first bad at = 10191
CRATONVM_DBG_GC_STRESS=1048576 cratonvm --real-jdk … JitFrameRootRelocationProbe
#   nullFields = 67
CRATONVM_ZGC_RELOCATE=0        cratonvm --real-jdk … JitFrameRootRelocationProbe
#   nullFields = 0
```
