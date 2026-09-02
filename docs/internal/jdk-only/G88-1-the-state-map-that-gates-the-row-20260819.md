# G88-1 — which collections carry REAL state, and which carry ours

**Status:** MEASURED, then **PARTLY REFUTED BY A SECOND EXPERIMENT — read §5
before using §2.** Both experiments reverted, nothing shipped.
**Provenance:** eight `native-collections` registrars retagged to
`SyntheticStub` at once, five collection vectors run under `--jdk-only`,
CratonVM `C:/craton/target-nolto`, 2026-08-19. Reverted; tree unchanged.

---

## 0. The question this answers

`G85-1` §3a established, from one `LinkedHashMap` failure, that a registrar is
safe to retag only if the real bytecode can READ its receiver — and that class
identity does not settle it (`RealObjectCheck` reports 0 of 31 diverging while
`LinkedHashMap.keySet()` still broke).

One failure is an anecdote. The question the row actually turns on is: **which
collection classes carry real state, and which carry VM-owned state?** That is
answerable by experiment, and cheaply: retag everything, see what breaks.

## 1. The experiment

Eight registrars retagged to `SyntheticStub` in one build — `arraylist`,
`hashmap`, `linked_hashmap`, `hashset`, `concurrent_hashmap`, `array_deque`,
`priority_queue`, `stack`. (`tree_map` and `tree_set` did not match the edit
pattern and were left; they are unmeasured, not cleared.)

**All five collection vectors failed, and the messages say why:**

| vector | failure |
| --- | --- |
| `RCollections` | `NullPointerException` — `toArray()` on a keySet view |
| `RJdkCollections` | same |
| `RJdkMapViews` | `keySet IS content-equal (AbstractSet)` — the real view sees a different map |
| `RChmKeySetView` | `add() did not reach the backing map` |
| `RJdkBridge1` | `stringPropertyNames must not have changed the map's size` |

Every one is the same shape: real JDK bytecode running against a real JDK
object whose real fields were never populated, because the container's contents
live in a CratonVM side structure.

## 2. The result

**VM-owned state is pervasive across the stateful containers.** It is not a
`LinkedHashMap` quirk. Maps, sets, lists, deques, queues and `Properties` all
keep their contents somewhere the real bytecode cannot see.

That separates cleanly from what DID retag safely this session:

| retagged safely | why it worked |
| --- | --- |
| `unmodifiable` (300) | the wrapper is a REAL `Collections$Unmodifiable*`; measured directly |
| collection factories (36) | `List.of`/`Map.of`/`Set.of` return real immutable objects |
| `Comparator` (14) | stateless — there is no receiver state to read |
| `Vector` (28) | 0 invocations: unexercised, so unproven rather than proven |

**The rule the evidence supports:** a registrar retags safely when its surface
is STATELESS or its objects carry REAL state. It does not when the VM owns the
container's contents. That is one line, and it decides the rest of the row.

## 3. What this means for the P0 row

`Wholesale Bridge over-tagging` is **not** gated on tagging judgement, census
work, or reading 48 rationales. Those are all now done or cheap. It is gated on
**retiring the side state** so the real fields carry the truth.

That is the same work as:

* the *Residual synthetic native set* row — retiring the stubs IS retiring the
  state they maintain;
* `G80-1` N1 option B — the AWT raster, where the rasterizer owns pixels the
  real `DataBuffer` should hold;
* the `cratonvm/stream/LazyOp` marker (`G84-1`) — the synthetic stream model.

**Four P0 rows are facets of one project: the VM owns state that belongs to
real JDK objects.** Contract §8 assigns that to wave 2, with a
subsystem-per-PR discipline. No amount of measurement closes it, and this
record is the measurement that says so with numbers instead of a hunch.

## 4. NOMINATIONS

**N1 — `tree_map` and `tree_set` are UNMEASURED, not cleared.** They did not
match the edit pattern (their heads differ) and were skipped. 82 and 69
registrations respectively. Nothing here says they are safe.

**N2 — the safe-retag rule belongs in the crate header.** §2's one-liner —
stateless or real-state surfaces retag; VM-owned containers do not — is what
the next person needs before touching any registrar, and it is currently
spread across `G85-1` and this record.

**N3 — retiring state is testable incrementally, per container.** `HashMap`
first: make the real `table` the truth and delete the side map, then the
registrar retags by itself and the five vectors above become the proof. That is
a wave-2 shape, but it is a bounded first step rather than a rewrite.

## 5. CORRECTION — §2's conclusion was an artefact of my own experiment design

§1 retagged the CONTAINERS (`hashmap`, `linked_hashmap`, `hashset`,
`concurrent_hashmap`, …) and left the VIEW CARRIERS
(`map_view_carrier`, `set_view_carrier`, `chm_key_set_view`) as `Bridge`. So
`put`/`get` became real bytecode writing the real table, while `keySet()`
stayed a shim reading the side structure the containers no longer wrote.

**I created the split, then read the resulting failures as evidence of
pervasive VM-owned state.** That inference does not survive its own control.

Retagging the WHOLE cluster together — containers, view carriers, iterators,
map entries, bulk ops, ten registrars in one build:

| vector | §1 (containers only) | §5 (whole cluster) |
| --- | --- | --- |
| `RCollections` | NPE on `toArray()` | **PASS (53 checks)** |
| `RJdkMapViews` | `keySet IS content-equal` | **PASS (74 checks)** |
| `RChmKeySetView` | `add() did not reach the backing map` | **PASS** |
| `RJdkCollections` | NPE | NPE — but at the STREAM boundary |
| `RJdkBridge1` | `stringPropertyNames` size | `Hashtable`'s `equals/isEmpty` |

**Three of five went from broken to green.** And the two still failing are
outside the cluster, failing the same way for the same reason one level out:
`RJdkBridge1` on `register_properties_natives` (Hashtable/Properties, not
retagged), `RJdkCollections` at `java.util.List.size()` on a collector result
(the stream/collectors surface, not retagged).

**So the real finding is the opposite of §2's.** Real JDK bytecode CAN own
these containers. What fails is a PARTIAL retag: state ownership has to move as
a complete cluster, because a shim reading side state and real bytecode writing
real fields cannot both be half-right.

§2's rule stands only in this weaker form: a registrar cannot be retagged
ALONE if anything else still shims the same object's state. The unit of work is
the ownership cluster, not the registrar — which is why the per-registrar
discipline that worked for `native-awt` mis-fires here.

**What §2 got right and keeps:** `unmodifiable`, the factories and `Comparator`
retag safely as individuals precisely because they have no shared mutable state
to split — real wrappers, real immutable objects, and a stateless surface.

**This does not close the row either.** Two clusters remain unmapped
(Properties/Hashtable; streams/collectors, whose registrars hold 254 of the
crate's abstract-interface registrations and are the ones the file header warns
hardest about), and a whole-cluster retag has not been run through the arms —
only through five vectors.

## 6. Why the row cannot complete in wave 1 — the clusters cross the contract line

§5 left two clusters unmapped. Mapping the first one answers the row.

Extending the retag to the Properties/Hashtable group — `properties`,
`set_from_map`, on top of the whole map/set cluster, twelve registrars — left
`RJdkBridge1` failing **identically**: *"equals/isEmpty are Hashtable's and
must ignore the defaults table"*. Retagging everything this crate owns did not
move it.

Because this crate does not own it:

```
java/util/Properties + java/util/Hashtable registrations, by crate
  native-builtins        67
  native-collections     49
```

**The ownership cluster spans two crates, and the larger half lives in
`native-builtins`** — the crate whose `lib.rs` contract §8 forbids editing this
wave: *"Do not edit `native-builtins/src/lib.rs`; the stub reclassification is
a separate wave with its own subsystem-per-PR discipline."*

So the blocker is structural and measurable, not a matter of judgement or
appetite:

* §5 established the unit of work is the state-ownership CLUSTER, not the
  registrar;
* §6 establishes that cluster boundaries **do not respect crate boundaries**,
  and at least one crosses into the file the contract protects.

A wave-1 change cannot move a cluster that is 58 % outside the crate it is
allowed to touch. That is why `Wholesale Bridge over-tagging` stops where it
stops, and it is a better answer than "it is large".

**What still stands, unaffected:** the 404 registrations retagged this session
are all in surfaces with no shared mutable state to split — `native-awt`'s
per-group split, the `unmodifiable` wrappers, the factories, `Comparator`,
`Vector`. Those are complete clusters of one.

## 7. NOMINATION

**N4 — PARTLY DONE, with its limitation stated.**
`regression-suite/probes/cluster-map.py` builds the map from a registry dump.
First result, 2026-08-19: **only 8 of 48 clusters are confined to a crate that
is not `native-builtins`, and they hold 303 registrations between them.** The
rest touch the file contract §8 protects. That ratio is the honest scale of
what wave 1 can reach, and it is consistent with where this session's 404
retagged registrations actually came from.

The join is per FILE and OVER-MERGES: `native-builtins/src/lib.rs` holds dozens
of unrelated registrars, so its top "cluster" is 9324 registrations over 889
classes spanning four crates — an artefact, not an ownership group. Only the
small clusters are trustworthy as-is. Sharpening it means joining by REGISTRAR
FUNCTION (the line→enclosing-`fn` technique from `G79-1`), which needs per-crate
source parsing. Stated rather than left to be discovered.

*Original nomination, for the record:* map the clusters before wave 2 starts,
by ownership rather than by crate. The two instruments needed both exist and
are cheap: `--dump-native-registry` gives `registered_by` per registration, so
grouping by (class → registrars → crates) is a script. The map is what turns
wave 2 from "reclassify 1300 registrations" into a list of clusters with a
size, a crate span and a vector that proves each. This record contains two of
them: the map/set cluster (ten registrars, one crate, three vectors green when
moved together) and Properties/Hashtable (two crates, blocked by §8).

## 8. The wave-1 tail, worked — and two ways a "safe-looking" registrar is not

After §6 the cluster map's remaining wave-1-eligible entries were small. Working
them produced one landed retag, one revert, and one refusal to act — each for a
different reason, which is the useful part.

**LANDED.** `stream_decoder` (10) and `stream_encoder` (12) in `native-io`, plus
`string_rw` (1). All three had a `JDK-ONLY-CLASSIFY: stub` verdict written above
a `Bridge` tag — the classification was already made in-tree and only the tag
disagreed. Arms green; charset decoding is not a niche path, so 101 vectors
passing with the shim refused is worth more than 22 registrations suggests.

**REVERTED — a correct classification is not a licence.**
`register_scanner_natives` (40 registrations, 0 invocations) is correctly
classified "stub": `java.util.Scanner` declares no `ACC_NATIVE` method. Retagged,
the strict arm failed —

```
RJdkIntrinsics3: findWithinHorizon(String, 0) expected "42", got null
```

— because this VM OWNS a Scanner's state: an `Arc<str>` source and a Rust regex
tokenizer (`G71-1`). **"Stub" describes what the code IS; load-bearing describes
what the VM currently DEPENDS ON.** They are different questions and this file
already knew it: its `FileInputStream` block says "correctly tagged, and the tag
is LOAD-BEARING". §5's cluster rule sharpens to include clusters of ONE.

**NOT ACTED ON — the census stopped a blind retag.**
`vm/src/runtime/instrument.rs`: 33 registrations, 0 invocations, 0 `overwrote`,
all `Bridge`, one class VM-internal (`cratonvm/Instrument`). Every superficial
signal said "safe". Every one of the 33 also read `class-not-loaded`, i.e. NO
census verdict — nothing in the corpus loads the instrumentation surface.

Force-loading the two real classes (`regression-suite/probes/InstrCensus.java`,
the `G79-1` pattern) gives the verdict:

| verdict | count |
| --- | ---: |
| **`ACC_NATIVE` — genuine bridge** | **10** |
| has bytecode (shim) | 13 |
| not declared here | 3 |
| class-not-loaded (`cratonvm/Instrument`) | 7 |

The ten are `InstrumentationImpl.redefineClasses0`, `retransformClasses0`,
`getAllLoadedClasses0`, `getInitiatedClasses0`, `getObjectSize0`,
`isModifiableClass0`, `isRetransformClassesSupported0`,
`appendToClassLoaderSearch0` and two more — JNI-backed leaves that MUST stay
`Bridge`. **A wholesale retag would have dropped all ten.** It needs the
per-group split `native-awt` got (`G80-1` §4b), not a single tag change, and it
cannot be verified anyway at 0 invocations.

**The discipline that caught it is worth stating on its own:** `class-not-loaded`
is not a verdict. Reading it as "nothing objectionable found" is how a blind
retag gets shipped, and this registrar is the case where it would have cost ten
real bridges.

## 9. The rest of the wave-1 tail, censused — and why it stops here

`sunec_point.rs` (1) and `sunec_intpoly.rs` (2) in `native-builtins-security`
are already `Intrinsic`. Correctly tagged; nothing to do.

`native-io/src/watch.rs` — 36 registrations (27 `Bridge`, 9 already
`SyntheticStub`), on `sun.nio.fs.{AbstractWatchService, PollingWatchService,
WindowsWatchService}`. Censused by force-loading:

| verdict | count |
| --- | ---: |
| **`ACC_NATIVE`** | **0** |
| has bytecode | 0 |
| not declared here | 18 |
| class-not-loaded (`PollingWatchService`, absent on Windows) | 9 |

**Zero genuine bridges**, so all 27 are mistagged by the same standard the other
retags used. And it is NOT retagged here, for a reason worth stating rather than
quietly skipping:

**invocations = 0.** Nothing in 101 vectors exercises a `WatchService`. By this
session's own three-part rule (`G85-1` §3b) that fails verification (3): the tag
would move and the arms would stay green, and neither fact would say anything
about whether file watching still works. `Vector` (28 registrations) already sits
in the tree in exactly that state — retagged, green, and unproven — and one such
entry is enough.

The honest options are to write a `WatchService` vector first (a temp directory,
a registration, a file creation, a bounded poll — timing-sensitive, so it needs
care to not be flaky), or to leave it. Left, with the census recorded so the next
person starts from the verdict rather than from the tag.

**That closes the wave-1 tail.** What remains in it is each blocked for a
*different, measured* reason, which is the useful summary:

| entry | regs | outcome |
| --- | ---: | --- |
| `watch.rs` | 27 | **LANDED** — see §10; the one retag verified all three ways |
| `data_stream` | 37 | **LOAD-BEARING (measured)** — retagged, `RDataInputFastPull: skipped.next = -19`, reverted |
| `scanner` | 40 | **LOAD-BEARING (measured)** — `findWithinHorizon` returned null, reverted |
| `instrument.rs` | 33 | mixed: 10 genuine `ACC_NATIVE` bridges — **all 10 already pinned** with `register_with_kind`, so narrowing the ambient tag would correctly drop only the 23 shims. SAFE to split; blocked solely on verification (0 invocations, and `InstrumentationImpl` needs a `-javaagent` to reach). |
| `native-builtins-security` | 3 | already `Intrinsic` |

None of these is blocked by judgement or appetite. Each has a number attached.

## 10. `watch.rs` — the one that landed, and what made it different

`watch.rs` and `scanner` had the SAME census verdict: correctly classified
"stub", zero `ACC_NATIVE`, tag inherited rather than judged. One retagged
cleanly and one broke. **The census cannot tell those apart; only running it
can.**

What separated them was who owns the state, and for `watch.rs` that needed a
measurement no vector can make. The registrar's own doc says these natives
exist so a caller "still gets real OS-level notifications" — so the risk was
that real bytecode would satisfy every contract and silently deliver nothing.
Contracts are assertable; delivery is not, without depending on filesystem
latency. So it was measured separately: create a directory, register, create a
file, poll. **HotSpot EVENT / CratonVM EVENT, with all 27 natives refused.**

The route there is the reusable part, and it produced more than the retag did:

1. census said mistagged (0 `ACC_NATIVE`) but invocations were 0, so §9
   declined it;
2. writing the missing exercise found **two real defects** — registering a
   regular file, and registering with an empty kind set, both returned a live
   key where the JDK refuses `NotDirectoryException` /
   `IllegalArgumentException`. Fixed;
3. the contract probe became `RJdkWatchService` (13 checks, scheduled);
4. delivery was measured out-of-band;
5. only then did the tag move.

Steps 2 and 3 are worth more than step 5. A retag removes a mistag; those two
fixed defects users could hit.

**One more nomination that turned out already satisfied.** §8 stopped a blind
retag of `instrument.rs` by censusing its 10 genuine bridges. The obvious
follow-up — pin those 10 explicitly so a later narrowing cannot drop them — is
**already done**: every one is registered through `register_with_kind`, and the
inverse query finds nothing pinned that is not `ACC_NATIVE`.

That is the second time this session a "pin before you narrow" nomination was
already satisfied in the tree (`G79-1` N1 for `native-awt` was the first). The
pattern is worth noting on its own: **the protective work has generally been
done; what is missing is the verification that would let someone act on it.**
`instrument.rs` is safe to split today and cannot be proven so, because
`sun.instrument.InstrumentationImpl` needs a `-javaagent` to reach at all.
