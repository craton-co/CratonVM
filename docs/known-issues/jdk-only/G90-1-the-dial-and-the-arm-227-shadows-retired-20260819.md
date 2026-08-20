# G90-1 — the dial answered a question the census could not: 227 §1.4 shadows retired

**Date** 2026-08-19
**Subject** `native-api/src/retired_shadow.rs`, P0 *Wholesale `Bridge` over-tagging*
**Instrument** `CRATONVM_ENFORCE_NATIVE_SHADOW`, `--jdk-only-report`, `--dump-native-registry`
**Acceptance** `regression-suite/run.sh`, `CRATONVM_ARGS=--jdk-only`, 102 vectors

---

## 1. The number the row was arguing about was one vector's

The P0 *Wholesale `Bridge` over-tagging* row, and `G84-1` after it, quote
`native-shadows-bytecode: 104` — natives that stand in front of real JDK
bytecode under `--jdk-only`. That is one run of `RJdkStrict`.

Run all 36 vectors of the `--jdk-only` corpus and union the triples:

| | |
|---|---:|
| `native-shadows-bytecode`, **`outcome: native-won`** — the native RAN instead of the bytecode | **980** |
| of which `RJdkStrict` alone sees | 77 |
| reports whose 256-entry observation sink SATURATED | 3 of 36 |

So the figure in the row is low by an order of magnitude, and 980 is itself a
floor because three vectors filled the sink.

The `outcome` field is the part worth keeping. Of `RJdkStrict`'s 104
observations, 27 are `bytecode-won` — the native lost, which is the contract
working — and 77 are `native-won`. **A count of "shadows" that does not split on
outcome counts the successes with the failures.** Only the second number is a
defect.

## 2. The instrument that decides it already existed, and had been read as settled

`CRATONVM_ENFORCE_NATIVE_SHADOW` makes §1.4's rule enforced instead of counted:
a shadowing native yields and the real bytecode runs. `retired_shadow.rs`
records the whole-VM result — the strict corpus goes from 32 passed / 17 failed
to **3 / 46** — and concludes, correctly, that under `--jdk-only` the surviving
bridges *are* the object model for large parts of `java.base`.

That is a verdict on the whole population. It had been carried as a verdict on
every part of it, and the two waves since (logging, then a slice of collections)
were each argued from the census rather than from the dial.

The dial takes a **prefix list**. Nobody had swept it.

## 3. The sweep

One binary, the 36-vector `--jdk-only` corpus, baseline 36/36, only the dial
differing:

```text
  all                              1 / 36     <- reproduces the documented 3/46
  jdk/internal/access/            22 / 36
  java/security/                  33 / 36
  java/net/  java/nio/channels/  sun/nio/ch/            34 / 36
  java/math/  javax/crypto/  javax/management/
    javax/net/ssl/  sun/security/ssl/
    java/awt/image/  java/nio/file/                     35 / 36

  java/text/                      36 / 36
  java/util/stream/               36 / 36
  java/lang/module/               36 / 36
  java/util/concurrent/atomic/    36 / 36
  java/util/concurrent/locks/     36 / 36
  java/lang/ref/                  36 / 36
  sun/nio/fs/                     36 / 36

  all seven together              36 / 36
```

"1 / 36" and "36 / 36" come from the same dial on the same binary. The
catastrophe is real and it is **not evenly distributed**, which is the finding:
the whole-VM number could never have said so, and it is the only number anyone
had.

Running the seven together was a separate run, not an inference — a yielded
`Collectors` handing a real collector into a yielded `stream/` pipeline is a
pairing no single-prefix run exercises.

## 4. Then the census, to say which rows

The dial yields at dispatch *when bytecode is available*; retirement drops the
registration outright. Those agree only where bytecode exists, so the table is
the census-eligible subset, not everything under the prefixes. Of 522
registrations on receivers under the seven — 512 `Bridge`, 10 `Intrinsic`:

```text
  255  eligible   bridge, owns_slot, class LOADED, image target has Code,
                  and NOT ACC_NATIVE
  195  held       loaded, but no Code on the target — dropping these replaces
                  a shadow with an UnsatisfiedLinkError
   40  held       class never loaded in 36 vectors. `class-not-loaded` is the
                  absence of a verdict, not a verdict (G88-1)
    7  held       genuinely ACC_NATIVE — §1.5 bridges, correct as they are
   10  held       Intrinsic, out of scope for a §1.4 shadow
```

Each of those three "held" reasons is a trap this project has already fallen
into once, which is why they are counted in the record rather than filtered
away in a script.

## 5. THE SCREEN PASSED TWO PREFIXES THE ARM REJECTED

All 255 went in, and `regression-suite/run.sh` with `CRATONVM_ARGS=--jdk-only`
came back **99 / 102** against a 102 / 102 baseline.

```text
  RFileTimes           plain.readAttributes.lastModified
                         HotSpot   2021-01-01T00:00:00Z
                         CratonVM  1601-01-02T20:42:25.920Z
  RClassUnloadSweep    payload.class.unloaded
  RClassUnloadSweepGen   HotSpot true / CratonVM false
```

Both diffs name their own cause. **1601 is the Windows FILETIME epoch**: real
`WindowsFileAttributes` bytecode read its own time fields and found them at
zero, because the VM had been answering from side state and never populated
them. And a `Reference` that yields to real bytecode stops reporting the
clearing `RClassUnloadSweep` detects unloading by.

Same shape both times, and the same shape as every dirty prefix in §3: **the VM
owns state that belongs to the real object.** That is `G88-1` §5 reached from
the other direction — by execution instead of inspection.

### Why the screen missed it, and why that matters more than the two prefixes

The 36-vector corpus **has no file-attribute vector and no class-unloading
vector**. For those two subsystems it asked nothing and reported a pass.

That is a population narrower than the claim drawn from it — the third instance
of that exact defect found today, after the stub ratchet's CI wiring running one
of two frozen configurations (`G89-1` §4) and, before that, twice inside the
ratchet file itself. It is cheap to make, it always reads as success, and
nothing catches it but running the wider thing.

The useful corollary is about the *shape* argument, not the corpus. A
`Reference` is four fields; a `WindowsFileAttributes` is a handful of longs.
Both LOOK exactly as stateless as an `AtomicInteger`. Nothing about their shape
says the VM is answering for them. **"Stateless subsystems retire cleanly" is a
place to look, never a rule to apply** — and this is the wave that proves it,
having been named after the rule and then had two sevenths of it removed by
measurement.

## 6. What landed

`RETIRED_SHADOW_STATELESS_TRIPLES` — **227 triples over five prefixes**, a
second sorted table beside the existing 102 rather than merged into them (the
first table's per-block commentary explains individual holdbacks that a global
re-sort would scatter; and these were adjudicated dial-first, census-second,
which is worth seeing at a glance).

```text
  java/util/concurrent/atomic/   125    AtomicInteger/Long/Reference, the
                                        Array and Adder families, Markable
                                        and Stamped references
  java/util/stream/               60    Collectors 34, the Stream interfaces
  java/lang/module/               28    ModuleDescriptor + Exports/Opens/
                                        Provides/Requires
  java/util/concurrent/locks/     11    LockSupport, the AQS bases
  java/text/                       3    DecimalFormatSymbols, ParseException
```

Five new gates, each one a failure mode that would otherwise be silent:
sortedness, reachability through the predicate, disjointness from the first
table, a vacuity floor, and
`the_stateless_table_stays_inside_the_five_measured_prefixes` — keyed to the
**arm** and not to the screen, precisely because the screen passed two prefixes
the arm failed.

Measured effect, 36 vectors, before → after:

```text
  native-won shadow triples      980 -> 943   (53 removed, 51 under the five)
  stub ratchet, no-management   1386 -> 1611  (+225, rows UNCHANGED at 12792)
  stub ratchet, management      1396 -> 1622  (+226, rows UNCHANGED at 13160)

  regression-suite --jdk-only    102 / 102    (baseline, restored)
  SUITE=all / SUITE=core         unchanged — a retirement is a SyntheticStub,
                                 which registers and dispatches normally in
                                 compatible mode
```

The ratchet lines are the two-column rule from `G89-1` §2 reading its own first
real case, one day old. The count rose by 226 — the largest single rise this
constant has taken — and the registry did not grow by one row, which classifies
the whole delta as relabelling rather than new fakes without anyone having to
remember what the change did. The *157 → 165* note argues the identical
conclusion about eight registrations in eight paragraphs of prose, because at
the time there was no second number to point at.

The 16 shadow triples that appear only *after* the retirement are not a
regression. Retiring a bridge lets real bytecode run, and that bytecode reaches
further real code some *other* bridge shadows — `jdk/internal/module/Builder`
shows up exactly because real `ModuleDescriptor` now runs. The surface is being
uncovered, not created.

## 7. What this does not do

**It does not close the P0 row.** 943 `native-won` shadows remain across 36
vectors, three of the reports still saturate a 256-entry sink, and the largest
remaining populations sit in `native-collections/src/lib.rs` (249) and
`native-builtins/src/lib.rs` (161) — the second of which contract §8 reserves
for wave 2. What changed is that the row now has a measured population instead
of a one-vector figure, a method that produces retirements, and 227 fewer of
them.

**It does not make the shape argument a rule.** See §5.

## 8. Nominations

* **N1** — raise the observation sink cap (`--jdk-only-report`'s 256) or make
  saturation loud. Three of 36 reports saturated before this change and three
  after; every count in this record is a floor for that reason, and nothing in
  the output says so except a boolean nobody reads. Note what that means for
  the headline: 980 → 943 is a difference of two censored measurements, and the
  53-removed / 16-appeared split beneath it is the honest part.
* **N2** — populate `WindowsFileAttributes`' real time fields and
  `Reference`'s real state, then re-run the arm with those two prefixes armed.
  The diffs in §5 are the acceptance criteria, already written.
* **N3** — sweep the dial over the prefixes NOT tried here. §3 covers 18; the
  980 spans 51 packages. The cheap ones left are `java/lang/invoke/` (56),
  `java/lang/reflect/` (53), `java/io/` (69) and `jdk/internal/misc/` (35).
* **N4** — the `35 / 36` band is one vector each. `java/math/`, `javax/crypto/`,
  `javax/management/`, `javax/net/ssl/`, `sun/security/ssl/`, `java/awt/image/`
  and `java/nio/file/` each cost exactly one, which means each is one
  diagnosable diff away from being retirable — the same distance `RFileTimes`
  was from telling us about the FILETIME epoch.
