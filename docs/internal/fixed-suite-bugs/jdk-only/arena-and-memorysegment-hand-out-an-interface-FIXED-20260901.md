# The interface-classed FFM family — FIXED, and `--jdk-only` is no longer the worse mode

Retires `known-issues/jdk-only/arena-and-memorysegment-hand-out-an-interface-and-jdk-only-is-the-worse-mode-20260829.md`.

## Status

**FIXED. Verified 2026-09-01 on dev `de4a07c4d`** against HotSpot 25.0.3+9, both
modes, using the page's own instruments.

That page asked a contract question and declined to answer it. It was answered —
by `the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`, in favour of
**"the VM's own internal allocation shape"** — and both halves of the defect
closed. This page records the verification, and two things the page had wrong.

## Measured

`apps/probes/FfmCarrierProbe.java`, 106 rows:

| | rows differing from HotSpot |
|---|---|
| compatible | **0 of 106** |
| `--jdk-only` | **0 of 106** |

The page's own table, which is what it was named for:

| receiver | HotSpot | compatible | `--jdk-only` |
|---|---|---|---|
| all four `Arena` factories | concrete | **concrete** | **concrete** |
| every `MemorySegment` door | concrete | **concrete** | **concrete** |

Every `class is concrete` row — `Arena.global`, `ofAuto`, `ofConfined`,
`ofShared`, `ofArray` byte/int/long, `NULL`, `arena.allocate`, `allocateFrom`,
`asSlice`, `asReadOnly`, `reinterpret` — is `true` in both modes.

**`--jdk-only` is no longer the worse mode.** `apps/probes/FfmSegmentSweep.java`,
199 rows, the companion record's own instrument:

| | then (2026-08-29) | now |
|---|---|---|
| compatible | 27 rows differ | **20** |
| `--jdk-only` | 47 rows differ | **20** |

The two modes are now **the same 20 rows**, byte for byte. The asymmetry the
page was named for is gone.

## What the 20 residual rows are — and why they are not this defect

All 20 are one shape, 10 `class` and 10 `superclass`. **None of them is an
`isInterface` row.**

```
  confined.allocate(16) class       HotSpot jdk.internal.foreign.NativeMemorySegmentImpl
                                    CratonVM cratonvm.internal.foreign.MemorySegmentImpl
  confined.allocate(16) superclass  HotSpot jdk.internal.foreign.AbstractMemorySegmentImpl
                                    CratonVM java.lang.Object
```

That is the **decided and accepted** consequence of the carrier being the VM's
own allocation shape. The decision page's third signal is explicit that the
carrier *deliberately* does not share `AbstractMemorySegmentImpl`'s layout — "a
stand-in mimics the shape it stands in for; this one has its own and was kept
that way on purpose". A program comparing `getClass().getName()` against a
JDK-internal name will see the difference, and that is the trade that was made,
not a defect left open.

So the identity residual is now exactly the carrier's own name and superclass,
it is identical in both modes, and it belongs to the decision page rather than
to this one.

## Two things the page had wrong

**1. Its §2 mechanism no longer holds.** The page said `craton_segment_class_id`
resolves through `try_ensure_synthetic_class` — "the door that mints
compatibility stand-ins … the one thing `--jdk-only` forbids" — so strict got
`None` and fell back to the interface stamp. That was true when written. The
code now calls `ensure_vm_internal_class` (`ClassOrigin::VmInternal`, legal in
every mode by contract §1 item 6), and carries the decision and its five signals
in a comment at the site.

**2. Its proposed `Arena` fix would have changed nothing.** The page said:

> All four factories (`panama.rs:786/795/808/823`) allocate with
> `"java/lang/foreign/Arena"`, the interface's own name, and giving them the
> carrier `MemorySegment` got would improve compatible mode …

`panama.rs` really does still allocate with the interface name at those sites —
**and those registrations are dead.** `--dump-native-registry` on a live run:

```
  class java/lang/foreign/Arena, name ofConfined
    registered_by  native-builtins/src/phases_late/foreign_ffm.rs:3067
    invocations    4
```

Registration is LAST-WRITE-WINS and `foreign_ffm.rs` owns all four factories —
which `panama.rs`'s own comment says out loud ("`--dump-native-registry` says
`phases_late/foreign_ffm.rs` owns all four `Arena` factories and this registrar
owns none of them, so a memo added here is inert"). `foreign_ffm` mints an arena
as `jdk/internal/foreign/ArenaImpl`, a **real** JDK class, which is why the
`Arena` half is concrete in both modes without anyone editing `panama.rs`.

Editing the four `panama.rs` sites would have been a no-op with a green
measurement next to it. **Before changing a registration, ask
`--dump-native-registry` which one is live** — the same lesson the
`java.util.Random` shadow produced on 2026-08-30, where three registrations of
one triple existed and only the last one ran.

## Repro

```bash
javac -d . apps/probes/FfmCarrierProbe.java apps/probes/FfmSegmentSweep.java
java --enable-native-access=ALL-UNNAMED -cp . FfmCarrierProbe     # HotSpot control
cratonvm --java-home <jdk25> -cp . FfmCarrierProbe                # compatible
cratonvm --java-home <jdk25> --jdk-only -cp . FfmCarrierProbe     # strict
```

`SegmentClassProbe.java`, the probe the retired page used, is still not in the
tree (`probes/` was deleted wholesale on 2026-08-29, `3b2901531`). Both probes
above cover the same ground from `apps/probes/`, and `FfmSegmentSweep` covers it
more thoroughly.

## Related

* `known-issues/jdk-only/the-ffm-carrier-is-the-vms-own-allocation-shape-20260829.md`
  — the decision this page's §3 asked for, with its five signals and its
  falsifiers. **The 20 residual rows belong there.**
* `known-issues/jdk-only/ffm-segment-surface-nine-behavioural-defects-and-the-interface-classed-family-20260829.md`
  — the companion record. Its §4 sizing (27 compatible / 47 strict) is now
  **20 / 20**; whoever owns that page may want to re-state it.
