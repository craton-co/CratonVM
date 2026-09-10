# After the carrier fix, the only thing the strict corpus still diverges on — on all four keys — is one load-fragile counter that a page from ten days ago already ruled unusable

**Status:** open, and it is a PROBE change, not a VM change.
**Found:** 2026-09-09, by re-freezing `21-linux` after the `JavaLangAccess`
carrier fix and finding that what was left was not a defect.
**Blocks:** the four strict-corpus baselines being empty, and the `vthreads`
section being a deterministic gate instead of one whose mint is a lottery.

## 1. What is left

After the carrier fix, `scripts/baselines/jdk-only-strict-corpus-21-linux.txt`
went from 10 rows to 2, and both are the same section:

```
JdkOnlyPlatformProbe/real/vthreads
JdkOnlyPlatformProbe/strict/vthreads
```

`25-linux` and `25-windows` carry two rows each and they are the same two.
`21-windows` carries them too. **Every strict-corpus key on every platform is
now, apart from rows that can never be re-minted, this one section.**

## 2. It is one cell

The whole `vthreads` line, all three arms, jdk 21:

```text
hotspot  vthreads join=true isVirtual=true/true name=craton-v1 latched=true
         completed=256 sum=65280 terminated=true handoffs=64 allJoined=true
         pinned=true/true tl=32 mainPlatform=true
real     ...                                     handoffs=57 ...
strict   ...                                     handoffs=57 ...
```

and jdk 25, same binary, same day: `64` / `62` / `56`. Every other field on the
line is identical in every arm. The divergent set is `handoffs=`, and nothing
else.

## 3. It is not a CratonVM property, and that is measured, not argued

`apps/probes/JdkOnlyPlatformProbe.java:376` starts 64 virtual threads on
`sq.poll(20, SECONDS)` and offers 64 items with `sq.offer(i, 20, SECONDS)`,
counting only the polls that returned a value — and discarding what `offer`
returned. Section 7 of
[`phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md`](phase-2-adjudicated-the-corpus-cannot-decide-a-retirement-20260830.md)
went at this with a purpose-built probe (`apps/probes/VtHandoffProbe.java`,
which counts refused offers and timed-out polls separately) and settled it:

```text
HS 64   CV 64   CV 64   HS 64   HS 64   CV 64   CV 64   HS 64
VtHandoffProbe: 0-diff over 512 handoffs in 8 rounds, both VMs
```

Its conclusion, verbatim in substance: the row is **load-fragile on BOTH VMs**,
the earlier 55/58 spread was overlapping probe instances rather than a property
of this VM, and *a differential probe with a load-fragile row cannot be used as
an oracle*. The host these 2026-09-09 numbers came off was at load average 3.7.

**So the row is doing the opposite of its job.** It is not reporting a defect,
it is reporting how busy the machine was, and it is doing so in the one place
the corpus treats as authoritative.

## 4. Why this is worth fixing rather than baselining forever

The `21-linux` baseline note records that minting it took three attempts and had
to be held to ten consecutive passing runs, because a mint taken from a run that
saw fewer rows reads the larger set as NEW and sends the leg red at random. That
whole hazard is this cell. Remove it and the `vthreads` section is deterministic,
the mint stops being a lottery, and three of the four keys go to zero rows.

## 5. What the fix has to preserve

`handoffs` is not decoration — it is the probe's evidence that blocking inside a
virtual thread UNMOUNTS instead of deadlocking the carrier, which is the
comment above that block. So it must not simply be deleted. Ask the SHAPE, which
is what `docs/contributing/jdk-only-lane-operations.md` requires of every probe
row:

* `handoffsPositive=` (some handoff completed, so the carrier did not deadlock),
  or
* `handoffsBounded=` (`0 <= handoffs <= 64`), or
* `allJoined=` alone, which is already on the line and already deterministic.

Whichever is chosen, **the acceptance test is that the section stops appearing
in `observed-keys.txt` on a LOADED host** — the condition under which it
currently diverges. Measuring it on an idle host proves nothing, because on an
idle host it already agrees.

## 6. Cost

Changing the probe changes what all four keys compare, so all four want
re-minting. Two of them can be: `21-linux` and `25-linux` on the azure Linux
host. `25-windows` and `21-windows` cannot be re-minted from this repo's build
machine (it has no JDK 21 for Windows), but that is harmless here — a row that
stops diverging reads as GONE and PASSES. The Windows keys simply stay two rows
looser, as they already are for the seven rows the 2026-09-09 fixes closed.

## Reproducing

```
JAVA_HOME=/data/jdkimages/jdk21-linux/jdk-21.0.12+8 \
CV=target/release/cratonvm \
OUT=target/strict scripts/jdk-only-strict-probes.sh
grep '^vthreads' target/strict/logs/JdkOnlyPlatformProbe.*.txt
```

The three lines differ in `handoffs=` and in nothing else. Run it again on an
idle host and they agree, which is the whole point.
