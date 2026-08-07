# The census asks one class, on one platform — and ~2,000 rows were filed under the wrong verdict

**Status:** the reading is fixed and the instruments ship. `undecl` is broken
out by `scripts/jdk-only-adjudicate.py --inherited`, the platform question is
answered by `scripts/jdk-only-platform-diff.py`, and the abstract-interception
blast radius by `scripts/jdk-only-interception.py`. **The dispositions are taken too** — see
*What the dispositions decided* — leaving one open item, which is a deletion
wave with a list attached rather than a question. Filed 2026-08-05
while closing the L5/L5b/L5c residuals; the three follow-ups were closed the
same day and are recorded under *What the follow-ups measured* below.

**Nothing here is a crash and no native's kind changed.** The defect is in what
the measurement *means*.

## The two questions `image_declaring_method` cannot answer

It resolves a registration's `(class, name, descriptor)` triple against the
bytes of **one class** in **one image**. Two things it therefore cannot see:

1. **An inherited declaration.** A native registered on
   `sun/nio/ch/SocketDispatcher.close(Ljava/io/FileDescriptor;)V` comes back
   `declared: false`. The method is concrete bytecode on
   `sun.nio.ch.UnixDispatcher`, two frames up, and CratonVM's receiver-driven
   dispatch finds the registration first — so the row is a contract §1.4
   **shadow of inherited bytecode**, and it was **dispatched three times** in
   the census run that reported it as undeclared.
2. **The other platform.** `jdk/net/WindowsSocketOptions.*`,
   `java/io/WinNTFileSystem.*` and `sun/awt/PlatformGraphicsInfo.hasDisplays0`
   score `ABSENT`/`UNDECL` on a Linux image for the same reason a genuinely
   dead registration does: the class or method is not in *that* image.

Both mistakes push rows into the bucket the adjudication script labels *class
present, method **not declared** — dead or misdescribed*, and both make a
correct registration look like a defect.

## What the instruments say

### Inheritance — `sh scripts/jdk-only-inherited-decl.sh <census.json>`

Runs `probes/InheritedDeclProbe.java` on real HotSpot against the same image,
resolving each `declared: false` triple up the class-and-interface chain.
JDK 25.0.4, Linux, 2026-08-05 — **2,542 rows in the bucket**:

| what it actually resolves to | rows | what the row really is |
|---|---:|---|
| inherited, concrete bytecode | 1,612 | a §1.4 **shadow** |
| inherited, abstract | 308 | intercepts **every** implementor, incl. user subclasses |
| inherited, `ACC_NATIVE` | **19** | a §1.5 **bridge the census did not credit** |
| genuinely nowhere in the hierarchy | 603 | a dead registration |

**76% of the bucket is not dead.** And the 19 are the sharp end: 18 are
`sun/nio/ch/FileDispatcherImpl.*` inheriting the syscall surface from
`UnixFileDispatcherImpl`, and one is
`java/awt/image/ComponentSampleModel.initIDs()V` inheriting a native `initIDs`
from `java.awt.image.SampleModel`.

### Platform — `python3 scripts/jdk-only-platform-diff.py <a> <b>`

The obstacle was assumed, not checked: **CratonVM adjudicates an image it
cannot run.** The pass parses class bytes off the module image and never
executes them, so a Windows JDK unpacked on the Linux host produces a complete,
image-adjudicated census. Two censuses, same binary, same workload, same JDK
version (25.0.4+7), only `--java-home` differing:

* **230 of 11,916 rows** get a different verdict.
* **59 rows are a genuine `ACC_NATIVE` bridge only on Windows** — all nine
  `WindowsSocketOptions` entries, the `WinNTFileSystem` family, and
  `PlatformGraphicsInfo.hasDisplays0`.
* **78 rows are one only on Linux** — the `UnixFileSystem` /
  `UnixFileDispatcherImpl` counterparts.
* **1,735 rows are `ABSENT` on both** — dead on every platform, and the honest
  deletion candidates.

## Three things this corrects in existing records

* **`sun/awt/PlatformGraphicsInfo.hasDisplays0()Z`.** Its
  `JDK-ONLY-CLASSIFY: bridge` marker was **right**, and L5b's rewrite of that
  marker — "bridge on Windows/macOS, DEAD on this image" — was right about the
  image and needlessly pessimistic about the verdict. It is a bridge; the Linux
  census simply could not say so. It states its kind now.
* **`sun/nio/ch/WindowsFileDispatcherImpl` exists on NEITHER image.** L5 left
  28 rows under that spelling inherited on the theory that they are "the same
  natives on their own image". They are not: the Windows JDK calls its class
  `FileDispatcherImpl` too. Those 28 are dead on every JDK 25 platform.
* **`java.io.RandomAccessFile.close0()V` and
  `sun/nio/ch/UnixFileDispatcherImpl.setDirect0(..CharBuffer..)I`** survive the
  wider reading: `NOT-FOUND` in the hierarchy, `ABSENT` on both platforms.
  The L5 record's verdict on them stands.

## What the follow-ups measured

### 1. `jdk-only-adjudicate.py` no longer lets `undecl` read as "dead"

Its header now states that the column is a superset, and
`--inherited <tsv>` adds a section **2b** that breaks the bucket into
`INHERITED code` / `INHERITED abstract` / `INHERITED native` / `NOT-FOUND`, and
lists the 19 rows section 2 counts as unadjudicated and should not. Without
the flag it prints, in place of the section, a line saying the split was not
run — an absent measurement announces itself rather than looking clean.

### 2. The abstract-interception blast radius, measured

`--dump-class-origins` rows now carry a **`supertypes`** column (direct
superclass and interfaces), which is the piece the join was missing: the native
census names the declaring class, the class census names every class the run
loaded, and only the supertypes edge connects them.
`scripts/jdk-only-interception.py` walks it.

On `JdkOnlyCensusLoadProbe` (JDK 25, 2026-08-05) — 705 classes loaded, 1,635
abstract-target natives, and **11 of them stand in front of a class the
application defined**:

| native | intercepts |
|---|---|
| `java/util/Collection.{isEmpty,iterator,size,toArray}` | `JdkOnlyCensusLoadProbe$MyCollection` |
| `java/util/Map.{containsKey,entrySet,get,keySet,put,size,values}` | `JdkOnlyCensusLoadProbe$MyMap` |

That is the `register_interface_natives` hazard, reproduced with a named
example instead of a worry: a user class implements `Map`, and eleven of its
methods resolve to a VM native written for `java.util`'s own implementations.
`invocations` is 0 for all eleven in this run — the probe builds the classes
but does not exercise every method — which is exactly why the surface matters
more than a receiver log.

**What it measures:** the interception *surface* — every loaded class that
inherits the intercepted method. Not a per-invocation receiver log. The VM's
dispatch sites do not uniformly hold the receiver, and threading one through
them would put work on the hottest path in the interpreter to buy a narrower
answer: the surface is what the registration *can* capture, and it does not
depend on whether this workload happened to call it.

### 3. "Dead on both images" was six times smaller than the count suggested

The 1,735 figure is **not** a deletion list, and this record said it was. Split
by namespace (`scripts/jdk-only-platform-diff.py` now prints this):

* **254** are in a JDK namespace (`java.`, `javax.`, `jdk.`, `sun.`,
  `com.sun.`) — names that should be in the image and are in neither. Those are
  the deletion candidates. Concentrated in `plain_socket.rs` (34),
  `atomic_updater.rs` (32), `nio_native.rs` (28), `native-collections` (21),
  `shared_secrets_bridge.rs` (18), `watch.rs` (18).
* **1,481** are third-party or VM-minted names — `org.springframework.`,
  `io.netty.`, `groovy.`, `cratonvm/synthetic/…`. They are absent from a JDK
  image *by construction* and live whenever the application supplies them.
  Deleting one because a JDK census called it `ABSENT` would remove a working
  native.

And the 254 carry their own caveat, which the tool now prints: **a registration
dead on JDK 25 may be the live one on JDK 21.** The diff sees one version. A
deletion wave has to sweep the versions the project supports first.

## What the dispositions decided

### The 1,612 inherited shadows: differential-tested, and one of them was wrong

Counting shadows says nothing about whether any is *wrong*, so
`probes/ShadowDifferentialProbe.java` exercises the most-reached shadowed
surface — `List/Set/Map.of`, `copyOf`, `unmodifiable*`, `subList`,
`LinkedHashMap`'s entry views including write-through `setValue`, and
`Map.entry` — and prints every observable so the run diffs byte-for-byte
against HotSpot 25.

**Everything matched except `Map.entry`, and it was wrong in two ways:**

| | HotSpot 25 | CratonVM (before) |
|---|---|---|
| `Map.entry("k", 7).toString()` | `k=7` | `java.util.Map$Entry@6c` |
| `.setValue(9)` | `UnsupportedOperationException` | **succeeded** |

The second is the dangerous one: `Map.entry` is specified to return an
immutable entry, and a caller that defensively mutates a copy got no signal
that it had mutated something nobody would read. **Disposition: deleted.**
`java.util.Map.entry` is a static interface method with ordinary bytecode, so
removing the registration is the whole fix — contract §1.4's "the real bytecode
wins", applied by removing the thing that was winning. Both probes now match
HotSpot byte for byte, and `stub_ratchet` fell 554 → 553.

That is the disposition shape for the rest of the population: *a shadow is
adjudicated by a differential, not by a count.* The probe stays in `probes/` as
the regression test.

### The 11 application-class interceptions: measured inert

`probes/UserImplementorInterceptProbe.java` hands the VM a `Map` and a
`Collection` the probe wrote itself, whose methods answer values no JDK
implementation ever would (`get` returns `"LOUD:" + key`, `size` returns 4242),
and calls them through the interface type — the exact shape the natives are
registered on.

**Every answer comes from the application's bytecode.** The interception
surface is real and the dispatch does not use it. So the eleven registrations
stay, with evidence rather than a worry attached, and the probe is the
regression test that keeps it true. (The one divergence this probe did show was
`Map.entry`'s `toString`, above — not an interception at all.)

### The 791 dead registrations: a list, not a guess

`scripts/jdk-only-dead-sweep.py` intersects a census per image — JDK 21.0.12
and 25.0.4, linux and windows — filters `UNDECL` through the hierarchy, and
subtracts everything three workloads dispatched (`JdkOnlyCensusLoadProbe`,
`JdkOnlyBreadthProbe`, and an H2 in-memory SQL workload; 787 slots between
them). What survives is committed at
`scripts/baselines/jdk-only-dead-everywhere.tsv`:

* **239** whose class no supported image contains;
* **552** whose method is nowhere in its hierarchy on any of them;
* concentrated in `lang_string.rs` (119), `plain_socket.rs` (51),
  `nio_native.rs` (45), `shared_secrets_bridge.rs` (44).

**Five more were removed by rule rather than by measurement** — see *The
fifth image* below. **The dispatch filter removed nine, and every one was a
VM-minted class wearing a JDK name** — `java/util/HashMap$KeyItr` (1,209 dispatches),
`java/util/TreeSet$Itr` (501), `AtomicIntegerFieldUpdater$RustJvmImpl`,
`Function$Identity`. A census saying a `java.util` class does not exist can be
right about the JDK and wrong about this VM, and that is the third distinct way
this measurement has been misread.

Deleting them is the stub-removal wave's job, not this one's. What changed is
that the wave now has a list with four images and three workloads behind it.

## The fifth image: the synthetic JDK, which the sweep never asks

The sweep censuses four **real** JDK images. It never censuses the one library
whose shapes CratonVM controls — its own. That is not a gap in coverage; it is a
question the instrument is structurally unable to answer, because the synthetic
JDK is not a JDK image and `image_declaring_method` has nothing to parse.

The consequence is a fourth way to misread this measurement, and it had already
put five rows on the deletion list:

| row | why the census called it dead |
|---|---|
| `java/util/Comparator$Native.compare`, `.writeReplace` | `$Native` is a class **this VM mints**. No JDK owes it. |
| `java/util/function/Function$Identity.andThen`, `.compose` | likewise minted. Its `apply` *was* dispatched and the filter caught it; these two were not exercised, so they stayed |
| `java/util/concurrent/locks/StampedLock.isLocked` | a real class, but the JDK declares no `isLocked()` — this is a deliberate completion of the synthetic surface, and `native-builtins/tests/registry_contracts.rs` pins it |

All five carry `kind: synthetic-stub`, which is the tell: a *synthetic stub is
CratonVM's own implementation*, so a census of JDK images can only ever report
that the JDK does not have it. That is agreement, not evidence.

**The rule: a synthetic stub is never a deletion candidate. Gate it, never
delete it.** `scripts/jdk-only-dead-sweep.py` now enforces this rather than
leaving it to whoever reads the list — `synthetic-stub` rows are routed to a
separate *gated* section and never written to the deletion file.

Nothing needed changing in the VM to satisfy the rule, which is worth stating
plainly: `NativeKind::SyntheticStub` is **the one kind `--jdk-only` rejects**
(`NativeKind::allowed_in`), so all five were already gated — strict mode drops
them and the real bytecode wins. In real-JDK mode they are *inert* rather than
wrong: nothing can reach a class that exists only when the VM minted it, and
nothing can call a `StampedLock.isLocked()` the JDK never declared. The defect
was never in the code. It was in the list.

### Why "gate" and not "delete" is the load-bearing distinction

`21cfa930f` is the worked example. It deleted `native_map_entry` — whose body
was `alloc_synthetic(ctx, "java/util/Map$Entry", 2)` — and it was **right** to
stop it running: the differential caught `toString` returning
`java.util.Map$Entry@6c` instead of `k=7`, and `setValue` succeeding where the
spec requires `UnsupportedOperationException`.

But deletion satisfied one mode only. Under `--real-jdk` the JDK's bytecode
takes over and the behaviour becomes correct. Under `--synthetic-jdk` there is
no bytecode to take over, so the method may now be missing outright. A gate
would have served both; deleting served one and silently cost the other.

Note that `NativeKind::SyntheticStub` is **not** the right gate for that case:
`allowed_in(Compatible)` is `true` for every kind, so the tag alone would let
the native keep running in default real-JDK mode and reintroduce both
divergences. The correct gate there is the compile-time one the repo already
uses, `#[cfg(feature = "synthetic-jdk")]`.

### Measured 2026-08-06: it was not hypothetical

`21cfa930f` was verified against `--real-jdk` and the verification was sound
there. `Map.entry("k",7).getClass()` reads `java.util.KeyValueHolder` on both
HotSpot 25 and CratonVM, because the registry drops the two surviving
registrations when the real image supplies the method and `java.base`'s
bytecode runs. Both remaining natives are dead code in that mode.

Run under `--synthetic-jdk`, nothing is dropped, and the record's own
"before" column came back verbatim:

| | HotSpot 25 | CratonVM `--synthetic-jdk`, before |
|---|---|---|
| `Map.entry("k",7).toString()` | `k=7` | `java.util.Map$Entry@6c` |
| `.setValue(9)` | `UnsupportedOperationException` | succeeded |

`ShadowDifferentialProbe` is the regression test for this exact surface, and it
had only ever been pointed at one mode. Pointed at the other it diverged on 7
of its 42 lines — from two ABSENCES rather than seven bugs. Entry `toString`
was a placeholder printing the object's address (which is what
`LinkedHashMap.entrySet().toString()` emitted instead of `one=1;two=2;`), and
`equals`/`hashCode` were not registered at all, so entries fell back to
identity and two entries with equal keys and values compared unequal.

Registering the three specified methods on the entry classes took the synthetic
differential from **14 diverging lines to 2**, with `--real-jdk` byte-identical
to HotSpot throughout.

**The residual is instructive and is left open deliberately.**
`Map.entry(...).setValue(v)` still mutates instead of throwing, because
`java/util/Map$Entry` is ALSO minted as a three-field entry — `key@0, value@1,
sourceMap@2` — by the entry-set views in `native-collections` and
`properties_sidetable`, precisely so `Entry.setValue` writes through to the
backing map, which `entrySet()` iteration requires. `Map.entry`'s entry is
2-field and must throw: one synthetic class name, two contradictory contracts,
resolved by last-write-wins.

An immutable `setValue` registered for the second contract loses that race
today — and it must, since if it ever won, every `entrySet()` write-through
would break. Registering a native whose correctness depends on losing a race is
not a fix. The real fix is to give `Map.entry` a class of its own, which is why
HotSpot has `KeyValueHolder`; re-pointing the allocation at
`SimpleImmutableEntry` was measured as a regression (4 diverging lines to 6).

### What blocked the residual, and why the first reading of it was wrong

`ShadowDifferentialProbe` had never exercised an entry a *user* constructs —
every entry in it came from a native. Widening it to cover
`new AbstractMap.SimpleEntry<>(k, v)` and its immutable sibling took the
synthetic differential from 2 diverging lines to **9**: seven were hiding
behind an unprobed constructor.

| | HotSpot 25 | `--synthetic-jdk`, before |
|---|---|---|
| `new SimpleEntry<>("k", 7).getKey()` | `k` | `null` |
| `.toString()` | `k=7` | `null=null` |
| `.equals(an equal SimpleEntry)` | `true` | `false` |
| `.setValue(8)`, then `getValue()` | `8` | `null` |
| `new SimpleImmutableEntry<>("k", 7).toString()` | `k=7` | `entry@200c2c00400` |

Three causes, none of them in a native's logic:

1. **The fabricated entry classes declared ZERO field slots.** A bytecode `new`
   sizes its object from `num_total_fields`, so it allocated a 0-slot object
   and the native `<init>`'s two `set_field` writes were dropped by the heap's
   bounds guard. The VM said so on every one of them —
   `set_field: out-of-bounds field write dropped … num_slots=0
   class_name=java/util/AbstractMap$SimpleEntry` — into a WARN nobody was
   reading. Entries the MAP creates were unaffected, and that is what hid it:
   `alloc_synthetic` passes its own slot count straight to `alloc_object`, so a
   map-minted entry is well-sized whatever its class declares.

2. **They declared no interfaces, so they were not `Map.Entry`s.** This is the
   defect the previous entry in this file called "a native `equals` shadowed on
   classes with a synthetic method table", and that reading was wrong. The
   native ran every time. `Map.Entry.equals` is specified against *any other
   `Map.Entry`*, so every implementation of it opens with an
   `instanceof Map.Entry` test — and this VM's answer to that question, asked
   about this VM's own entry, was false. `toString` and `hashCode` looked
   healthy for the single reason that discriminates them: neither asks what
   type anything is.

   Two `equals` registrations exist for `SimpleEntry` —
   `native_entry_equals` in `native-collections` wins the last-write-wins race
   against `register_entry_value_semantics` in `native-builtins` — and only the
   winner type-tests. That is why three successive patches to the loser's body
   changed nothing, which was read at the time as evidence that no `equals` was
   running at all.

3. **`SimpleImmutableEntry.toString` was registered twice.** `fffc08a70` added
   the contract `toString` and left the older `entry@<address>` placeholder
   registered *below* it, so last-write-wins kept the placeholder. A duplicate
   introduced by the fix for duplicates.

Fixed the same day: `synthetic_stub_fields` models `key`/`value`,
`jdk_interfaces` links `Map$Entry` + `Serializable`, the placeholder is deleted,
and `register_entry_value_semantics`'s `equals` grew the type test its sibling
already had (without it, an entry compares equal to anything that answers
`getKey`/`getValue`). `abstract_map_entry_stand_ins_carry_two_slots_and_
implement_map_entry` pins both halves and fails on the pre-fix tree with
`left: 0, right: 2`.

**9 diverging lines to 1**, with `--real-jdk` byte-identical to HotSpot
throughout. The 1 is `Map.entry(...).setValue`, which is open item 3.

## What is still open

1. **Delete the 791.** The list is measured and committed; removing the
   registrations is a stub-removal change with its own subsystem-per-PR
   discipline, and it should re-run the sweep afterwards rather than trusting
   this file. Two cautions the list itself cannot carry: its `registered_by`
   line numbers are **already stale** against `dev`, so rows must be re-located
   by content; and the largest cluster (`lang_string.rs`, 119) is a covariant
   fan-out that registers each `append` under three return descriptors, of
   which only some (class, descriptor) pairs exist in any JDK — those are
   mechanical, but they are not representative of the rest.
2. **Restore what earlier waves deleted instead of gating.** `14145d874`
   removed five duplicate registrations that L7 R1's retag had missed;
   retagging them `SyntheticStub` was the available tool and deletion was used
   instead. `21cfa930f` deleted `native_map_entry` outright. Both should be
   restored behind gates rather than left deleted — the second under
   `#[cfg(feature = "synthetic-jdk")]`, so real-JDK mode keeps the
   HotSpot-correct bytecode and synthetic mode regains the method.
3. **`Map.entry(...).setValue()` is permissive under `--synthetic-jdk`** —
   now the only line of `ShadowDifferentialProbe` that diverges, and the two
   defects recorded here on 2026-08-06 as blocking it are fixed (see *What
   blocked the residual* above).

   The fix is unchanged: give `Map.entry` a class of its own. Minting
   `java/util/KeyValueHolder` — the JDK's own answer, and the class HotSpot
   returns — makes `setValue` throw, makes `getClass()` agree, and leaves
   `--real-jdk` byte-identical. It was measured on 2026-08-06 and **not
   landed**, because `SimpleEntry.equals(theKeyValueHolder)` answered false
   while the reverse answered true, and shipping an asymmetric `equals` to fix
   a permissive `setValue` is a trade, not a fix.

   That asymmetry was blocker 2 above — `SimpleEntry` was not a `Map.Entry` —
   and it is gone. A `KeyValueHolder` that declares `Map$Entry` in
   `jdk_interfaces` and two slots in `synthetic_stub_fields` should now be
   symmetric in both directions. **That has not been measured**, and it is the
   whole of what is left here.
4. **The differential covers what it covers.** `ShadowDifferentialProbe`
   exercises `java.util`'s factories and views. The other ~1,600 inherited
   shadows are unprobed, and the honest reading of "they match" is "the ones
   anybody looked at match". Widening that probe is the cheapest way to keep
   finding `Map.entry`-shaped defects.
5. **`jdk-only-adjudicate.py` section 3 now prints the inherited shadows as a
   separate addend** rather than folding them in, because L6's ratchet counts
   `has_code` on the named class and that number must keep meaning exactly
   that. Prose calling it "the shadows" still understates by ~1,600; the script
   now says so on every run.

## Reproducing

```sh
# the census (all three instruments consume it; add --dump-class-origins for
# the interception join)
cratonvm --real-jdk --java-home "$JAVA_HOME" --explain-jdk-only \
    --dump-native-registry census.json -cp probes L5rProbe

# 1. inheritance
JAVA_HOME=<same image> sh scripts/jdk-only-inherited-decl.sh census.json

# 2. platform — unpack a Windows JDK of the SAME version, then
cratonvm --real-jdk --java-home <windows-jdk> --explain-jdk-only \
    --dump-native-registry census-win.json -cp probes L5rProbe
python3 scripts/jdk-only-platform-diff.py census.json census-win.json linux windows

# 3. abstract-method interception, including application classes
python3 scripts/jdk-only-interception.py --registry census.json \
    --classes classes.json --inherited undecl-out.tsv --only-user

# and the adjudication table with the bucket broken out
python3 scripts/jdk-only-adjudicate.py census.json --inherited undecl-out.tsv
```

Both refuse rather than print zeroes when the census lacks
`--explain-jdk-only`, and the platform diff refuses if the two censuses do not
correspond row-for-row.
