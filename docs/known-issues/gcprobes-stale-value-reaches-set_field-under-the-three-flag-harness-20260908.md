# A stale VALUE reaches `set_field` under the three-flag stale-ObjectRef harness, at the same minor cycle every run

| | |
|---|---|
| **Status** | OPEN — reproducible and deterministic, NOT root-caused. |
| **Reproducer** | `probes/NativeLoopReceiverSweep.java`, the `growth` section alone |
| **Scope** | Generational only. G1 is unaffected. `-Xint` does not change it, so it is not the JIT. |
| **Pre-existing** | Yes — identical on `origin/dev` (`ff636f3c1`) and on the branch that retired the loop/launder pages. Not caused by either. |

## The command

```
CRATONVM_DBG_GC_STRESS=65536 CRATONVM_DBG_FORCE_MOVING=1 CRATONVM_DBG_STALE_OBJREF=1 \
  cratonvm --XX:UseGc Generational -Xmx256m -cp out NativeLoopReceiverSweep growth
```

```
[storechk] set_field: STALE stored VALUE 0x…01c0 (receiver 0x…0438 slot 0) thread=main-vm cycle=460 — canary panic follows
# SIGSEGV …
```

3 of 3 runs, and the minor-GC cycle number is **460 every time** — this is not a
race, it is a fixed point in the allocation sequence.

## It needs all THREE flags, which is what makes it interesting

| flags | result |
|---|---:|
| none | 3/3 pass |
| `GC_STRESS=65536` | 3/3 pass |
| `DBG_FORCE_MOVING` | 3/3 pass |
| `DBG_STALE_OBJREF` | 3/3 pass |
| `GC_STRESS` + `FORCE_MOVING` | 3/3 pass |
| all three | **0/3 — SIGSEGV** |

The quarantine is the third flag, and it is a DETECTOR: it holds evacuated young
arenas so a stale read faults instead of silently reading a forwarded header.
So the honest reading is that the first two flags create a stale reference the
default configuration tolerates, and the third makes it fatal. That is the same
relationship the retired loop/launder pages have to their own proof — the
detector does not manufacture the staleness, it refuses to absorb it — but here
the producing site is NOT identified, which is why this is its own page and not
a line in theirs.

## What is known

* The canary fires on `set_field`, producer-side: the VALUE being stored is
  already forwarded when the store happens. Slot 0 of the receiver.
* It happens BEFORE the section's first printed line, i.e. inside the
  `ConcurrentSkipListMap` fill loop or the `System.gc()` that follows it.
* **It does not reduce.** The same loop extracted verbatim into a standalone
  class (with and without the probe's method-reference dispatch, with and
  without its `check()` helper and its `System.gc()`) passes 3/3 under the same
  three flags. It needs something about the probe class itself — nine
  `invokedynamic` method-reference bootstraps ahead of the loop is the obvious
  suspect and is untested.
* Running the probe's `streams` section FIRST masks it (`streams growth` passes
  3/3), which is consistent with a bootstrap-machinery ordering dependency
  rather than anything in the collections themselves.

## What this does NOT establish

Nothing about the default configuration. Under plain `--XX:UseGc Generational`
with no debug flags the whole probe passes 5/5, on both binaries. The claim
here is narrow: a documented debug harness whose whole purpose is to find stale
references finds one, deterministically, and nobody has yet named the producer.

## Next

* Bisect the probe class down to the minimum that reproduces — start by
  deleting sections from `NativeLoopReceiverSweep` rather than by rebuilding
  the loop elsewhere, since rebuilding it is what already failed to reproduce.
* `CRATONVM_GC_RESERVE=0` keeps the granules mapped, so the same defect reads
  stale bytes instead of faulting; that turns the SIGSEGV into a value the
  probe can print and attribute.
* The `[storechk]` canary names the STORED VALUE. Its sibling in
  `set_array_element` is the one to watch if the bisection moves the fault.

## Addendum 2026-09-08: it does NOT reproduce on Windows, on either binary

Run on a Windows 11 box while
`natives-hold-a-stale-reference-across-a-park-FIXED-20260908.md` was being
verified, because the `growth` section is exactly the surface that page's fixes
touch (`CopyOnWriteArrayList.addIfAbsent`/`contains`/`addAll` and
`LinkedBlockingQueue.offer`, all of which held a reference across an allocation
or a park before it).

The command is this page's own, verbatim, `growth` alone:

| binary | runs | rc=139 | `[storechk]` lines |
|---|---:|---:|---:|
| `origin/dev` tip of 2026-09-07 20:29, **pre-fix** | 3 | **0** | 0 |
| the same tree **with the nine park/allocation fixes** | 3 | **0** | 0 |

Both print the same 18 rows and every one is `OK`; `diff` of the two outputs is
empty. So on this platform the harness does not produce the stale store at all,
and the fixes neither caused nor cured it — **this page is not retired, and its
reproducer is now known to be platform-dependent**, which its "3 of 3 runs, and
the minor-GC cycle number is 460 every time" reads as it not being.

The useful consequence for whoever picks this up: the bisection in "Next"
should run on the platform the original measurement came from, and the first
thing worth checking is whether `cycle=460` survives a different allocator
page-size / arena geometry at all. A fixed point in the allocation sequence is
a property of the sequence, and the sequence is not the same on both hosts.

What the two runs above DO establish, and it is worth keeping: the `growth`
section's answers are byte-identical before and after nine natives in that exact
call path were rewritten to re-read their references. That is the regression
check those rewrites needed.
