# JDK-only wave 2 — parallel execution plan

**Purpose:** finish `--jdk-only` (contract:
[`../jdk-only-mode.md`](../jdk-only-mode.md)) with as many people working at
once as the file layout allows. This is the *guide*; each lane has its own doc.

**This is not the defect list.** The evidence — what is broken, how it was
measured, blast radius — lives in
[`docs/known-issues/jdk-only/`](../../known-issues/jdk-only/) and stays there.
A record moves to `docs/internal/` when it is fixed. This directory says **who
can work on what, simultaneously, without colliding.**

---

## Start here

1. Read the contract's §1.3, §1.4, §5, §7 and §11. Every lane is judged against
   those, not against this plan.
2. Pick a lane from the table below whose **owned files** nobody else holds.
3. Read that lane's doc. It states its own current state, steps, verification
   and "done when".
4. Follow *Verification protocol* below. It is not optional and it is the part
   most likely to be skipped.

## The lanes

| Lane | What | Owned files (the parallelism contract) | Gated on | Effort |
|---|---|---|---|---|
| [L1](L1-classloader-side-table.md) **DONE 2026-08-05** | Move the four VM-internal loader fields out of the object | `native-builtins/src/classloader.rs`, `classloader_real.rs` | — | M |
| [L2](L2-native-map-init-by-name.md) **DONE 2026-08-04** | `native_map_init`'s raw `MAP_FIELD_*` branch → by-name | `native-collections/src/lib.rs` | — | M |
| [L3](L3-scanner-membername-residual.md) | Trace + fix the last unclassified layout rows | `native-builtins/src/phases_early.rs`, `lang_invoke.rs` | — | S |
| [L4](L4-overlay-detector-blind-spots.md) **DONE 2026-08-05** | Detector misses reads, same-kind writes, null writes | `vm/src/vm/vm_exec.rs` (hunter only), `classloading/src/shadow_layout.rs` | — | M |
| [L5](L5-nativekind-native-io.md) | `register_with_kind` migration, `native-io` first | `native-io/src/*.rs` | — | M |
| [L6](../../internal/L6-unadjudicated-bridge-ratchet-DONE-20260805.md) **DONE 2026-08-05** | Ratchet the unadjudicated `Bridge` rows — frozen at **10,069** (25/linux) | `regression-suite/`, `scripts/` | — | S |
| [L7](L7-ensure-synthetic-class-migration.md) | Make fabrication refusable, migrate the 3 live callers | `classloading/src/class_manager.rs` + callers | — | M |
| [L8](L8-strict-corpus-green.md) | Criterion 6: strict corpus green | `probes/`, `regression-suite/` | — | L |
| [L9](L9-blocker-rkc16n6-string.md) | ~~**Blocker.** Real `String` bytecode during JDK `<clinit>`~~ **CLOSED 2026-08-04** — did not reproduce; the four policy copies were measured inert and deleted | `vm/src/runtime/interpreter/` | — | L |
| [L10](L10-blocker-threadpool-init.md) | **Blocker.** Real `ThreadPoolExecutor` field init | `native-collections/src/lib.rs` ⚠ | — | L |
| [L11](L11-delete-the-hardcoded-lists.md) | Items 3 + 7: delete the lists — **item 3 DONE 2026-08-04** | `native_override.rs`, `vm_exec.rs` ⚠ | ~~L9~~, L10 | M |
| [L12](L12-item11-residuals.md) | Item 11 §2/§4/§6/§8/§9/§10/§11 | mixed — see doc | partly L5 | L |

**L3–L5, L7 and L8 can all start today, in parallel, by different people.**
(L1, L2 and L6 are done; L9 is closed.)

## Conflict matrix — read before claiming a second lane

Two lanes are safe together iff they own disjoint files. The collisions that
exist:

| Pair | Collides on | Resolution |
|---|---|---|
| L2 ↔ L10 | `native-collections/src/lib.rs` | **Resolved:** L2 landed 2026-08-04; L10 rebases onto it. |
| L4 ↔ L11 | `vm/src/vm/vm_exec.rs` | L4 owns the overlay hunter (~line 3070–3200); L11 owns dispatch (~14700, ~22700). Disjoint regions in one file — coordinate, do not both `git add -A`. |
| L3 ↔ L12 | `lang_invoke.rs` | L3 is a handful of lines; land it first. |
| L5/L6/L12 ↔ each other | `register_with_kind` semantics | **Resolved:** L6 landed 2026-08-05 and changed no call site — it reads the census and freezes two numbers. L5's migration now has to move them; re-freeze with `sh regression-suite/bridge-ratchet.sh --update-baseline --note "…"` in the same change. L12 §4 is JIT-side. |

`native-collections/src/lib.rs` is 55k lines and `vm_exec.rs` is 26k — two
people in either will conflict even in "different" areas. Treat whole-file
ownership as the unit.

## Verification protocol

Non-negotiable, because this codebase has repeatedly produced fixes that
measured as working and were not.

1. **A/B against the pre-fix binary, same workload, same run shape.** An
   aggregate over several probes and a single run are *not* a before/after —
   that error made an inert guard read as a working one on 2026-08-04.
2. **Both modes plus a HotSpot control.** `--jdk-only`, `--real-jdk`, and real
   HotSpot on the same probe. A divergence present in *both* CratonVM modes is
   not a strict-mode defect.
3. **Check exit status, not just stdout.** A timed-out run prints a truncated
   transcript that reads exactly like a short clean one.
4. **Gate the crate you actually edited.** An earlier gate set omitted
   `native-builtins` while fixing a `native-builtins` file.
5. **A guard must be shown to fail.** Inject the violation, watch it fail,
   revert. See *Failure modes* below.

Standing commands:

```sh
# layout defects
CRATONVM_DBG=overlay,overlay-all cratonvm --real-jdk --java-home $JDK -cp probes JdkOnlyCensusLoadProbe
# who wrote it (Rust frame + file:line; the Java frames mislead)
CRATONVM_DBG=overlay,overlay-all,overlay-bt=ClassName cratonvm ...
# native adjudication
cratonvm --real-jdk --java-home $JDK --explain-jdk-only --dump-native-registry c.json -cp probes JdkOnlyCensusLoadProbe
python3 scripts/jdk-only-adjudicate.py c.json     # section 7 is the ratchet's block
# ...and the gate over it (takes its own census; needs no probe)
JAVA_HOME=$JDK sh regression-suite/bridge-ratchet.sh
sh regression-suite/bridge-ratchet.sh --selftest  # hermetic, no VM, no JDK
# both at once
JAVA_HOME=$JDK CV=... PROBES=... scripts/jdk-only-measure-refusals-and-overlays.sh
```

## Failure modes this feature keeps producing

Every one of these has happened, most more than once. They are why the protocol
above looks paranoid.

* **A guard that cannot fail.** Three in one evening: a test that froze a
  divergence; `object_num_fields(x) >= N` (returns the requested count either
  way, so it cannot distinguish layouts); `slot < object_num_fields(this)`
  (stops an out-of-range write, not a wrong-field one). **Identify a layout by a
  field NAME the real class declares** — `vform`, `scheme`, `protocol`, `type` —
  never by count.
* **A number in a record is a claim, not a measurement.** Five were wrong:
  "about 8,000" registrations (11,909), "1,195 mis-tagged" (10,084), "52 call
  sites" (3 fire), "costs every inline-cached call" (zero), "sweep four crates"
  (24 named slots). Take the census before sizing anything.
* **The Java stack does not name the native writer.** The `ClassLoaders` rows
  show `BufferedWriter.initialBufferSize()`. Use `overlay-bt`.
* **A comment can describe control flow the code does not have.** §7 step 3
  claimed it fell through "past the JNI chain to the bytecode path"; that arm
  has three outcomes and none is the bytecode. Follow the `else` chain.
* **The shared build host lies.** Builds get OOM-killed (`SIGKILL`, never a
  compile error) and ssh drops mid-command. Check `MemAvailable` before
  building; treat load > 80 as invalidating; verify a background job exists
  rather than assuming your launch survived.
* **The census cannot see a same-kind wrong-field write, so a lane brief
  written from the census under-reports its own defect.** L1's step 4 said the
  three `ClassLoader` REFERENCE slots were safe because they "are already
  written by name too" — but the by-name write and the index write land on
  DIFFERENT fields (`name` is slot 1, and the index write put the parent
  ClassLoader there). The value-tag predicate only flags cross-type-class
  coercions, so zero of it appeared in any census.
  `classloader_parent` was returning the platform loader's own name String as
  its parent, and nothing measured it until a behavioural probe was diffed
  against the host JDK. **Read the writer against `javap` of the real class;
  the census is a floor, and for reference-into-reference it was a floor of
  zero.** *(Corrected 2026-08-05: L4's shadow-layout diff compares our model
  against the image by NAME and found 23 such slots on its first run. The floor
  is zero only where the model slot is anonymous — `_fN`, which asserts
  nothing.)*

## Definition of done (contract §11)

1. Strict registry contains zero `SyntheticStub` entries — **passes today**, but
   because `register()` refuses them at the door, not because they are gone.
2. Zero synthetic-stub invocations through any path.
3. Zero `CompatibilityClassRequested` violations on the corpus.
4. `Compatible` mode byte-for-byte unchanged.
5. No process globals for this feature's state.
6. **Strict corpus green.** The unmeasured half until 2026-08-04, and where the
   defects turned out to be — see L8.

## State as of 2026-08-05

Closed: items 8, 9, 10; item 11 §1 (answered — its cost is zero), §5
(retracted), §12, §13. Fixed outside the list: `--jdk-only` could not start a
thread; `MethodType.toString()`; eight missing system properties.

Item 2: 24 measured slots → **15 open**. Item 1: instrument built, migration not
started. Items 3/7: blocked on L9/L10.

**Update, later on 2026-08-04 — L2 landed.** The `Properties` family is gone
from the census (slots 2 and 3 → 0) along with the `HashMap` slot-2 `Int` over
`[`; `map_state`, `map_resize`, `resync_view_set`,
`hashmap_serialized_capacity` and both `HashMap` constructors resolve `table`
on the receiver instead of on a fixed `java/util/HashMap`. Re-measured with
the three standing probes plus the new `probes/MapLayoutMatrixProbe`: 6
classes / 15 slots before, 5 / 12 after — not comparable to the 24 above, see
the evidence record for why. `Compatible` corpus byte-identical.

~~Two things that block a clean read of item 2's remaining rows, both L4's:
a null written over a primitive is still invisible to the hunter, and a
same-kind wrong-slot write always was.~~ **Both closed 2026-08-05 — see the L4
update below.** Every count in item 2 is still a floor, now because three probes
are not Spring Boot rather than because the instrument is half-blind.

**Update, 2026-08-05 — L1 landed.** The eight `ClassLoaders` census rows are
gone (slots 0/3/4/6 on each built-in loader → 0), A/B'd against the pre-fix
binary on JDK 25 with every other row byte-identical, and loader identity is
pinned against HotSpot 25 by `probes/L1LoaderIdentityProbe`. It also took the
three same-kind REFERENCE slots its own brief had ruled out — which is the
second half of the floor warning above, now with a concrete instance: **it was
returning the platform loader's `name` String as its parent.** Two divergences
its probe found are filed as residuals in the lane doc, both pre-existing and
both outside its owned files (`isAssignableFrom` true across unrelated loaders;
a duplicate `defineClass` not raising `LinkageError`).

**Update, 2026-08-05 — L9 is closed and item 3 with it.** The blocker did not
reproduce: its April symptom was a class-load defect fixed the same month by
RKC16N.9, and all four copies of the forced-native `java/lang/String` policy it
was about turned out to decide nothing — `resolve_step1_native` dispatches a
registered native on the triple alone, before any of them runs, which a binary
with the lists deleted confirmed by producing a byte-identical 392-case
transcript and identical invocation counts. Two lanes were planned around a
comment. Item 7 is still blocked on L10.

That also starts item 1's migration: the four reviewed `java/lang/String`
fast-regex natives plus `hashCode` are `register_with_kind`'s **first callers**,
so `kind_stated` is no longer `false` on all 11,909 rows.

**Update, 2026-08-05 — L4 landed, and item 2's work list roughly quadrupled.**
The census goes from **4 distinct sites to 135** on the same three probes in
both modes, with every pre-fix row preserved at its count. Reads are
instrumented (70 read sites where there were none), `Object(None)` over a
primitive is flagged (two new write rows the pre-fix binary is silent on), and
the same-kind wrong-slot case is covered by a **shadow-layout diff**:
`synthetic_stub_fields` is the model the natives were written against, so when
the class also has real bytes the two layouts are diffed once at define time and
every disagreeing index reported.

That diff found **23 slots across 12 classes where our model names a different
field than the image declares** (152 disagreeing slots in all, across 73 of the
156 modelled classes the probes reach) — the kind-5 family this README's own lesson
below calls "a floor of zero". `java/lang/ThreadGroup` has both `name`/`parent`
and `daemon`/`maxPriority` **transposed**; `java/lang/Thread` slot 5 writes a
`ClassLoader` over `holder`; `ProtectionDomain`, `CodeSource`, the buffered
IO wrappers and `java/lang/reflect/{Field,Method,Constructor}` are all in the
list. None is fixed — each is its own change with its own A/B, and the list is
the lane's output. L4 also settled L3 step 3 in passing: `Scanner`'s model is
`instance_fields(5)`, and slots 3/4 are the real `delimPattern` /
`hasNextPattern`.

**The lesson below needs one correction, not a rewrite.** "For
reference-into-reference the census is a floor of zero" was true of a detector
that only compared value tags. Comparing our *model* against the image by NAME
is a different signal and it finds them. What stays unfindable is the case where
the model slot is anonymous (`_fN`) — there the model asserts nothing. Naming
more of `synthetic_stub_fields` is what shrinks that, and it is the follow-up.

**Update, 2026-08-05 — L6 landed; the `NativeKind` work is now measurable.**
`regression-suite/bridge-ratchet.sh` + `scripts/jdk-only-bridge-ratchet.py`
freeze the unadjudicated-`Bridge` population against
`scripts/baselines/jdk-only-bridge-ratchet.json`, slack-free, with a vacuity
floor. Frozen on dev `d010d611b4` / JDK 25.0.3 / linux at **10,069 of 10,842
`Bridge` rows with no `ACC_NATIVE` target**, of which **4,755 shadow concrete
bytecode** (separately ratcheted — that is the subgroup §7 step 3's decline can
reach). Re-measured, not copied: the brief's 10,084/10,844 was 2026-08-04, before
L1/L2/L9 and the `String` residuals.

Three things worth carrying into the other lanes:

* **The baseline key is `<jdk-feature>/<os>`, and a missing entry is a refusal
  (exit 2), never a pass.** The registrars are platform-conditional, so a Linux
  baseline scoring a Windows census is the mix `scripts/jdk-only-census.sh`'s
  header exists to prevent. Only `25/linux` is committed; three of the four
  `jdk-only` CI legs report themselves ungated rather than green.
* **`JdkOnlyCensusLoadProbe` is the wrong workload for a gate, and neither
  number needs it.** Registration happens in `SharedVm::new` and the image
  adjudication parses class-path bytes without loading anything, so a one-line
  probe yields a byte-identical block — verified, not assumed. The broad probe
  hung in its `net` section on 1 of 3 runs on a loaded host, and a hung probe
  writes no census.
* **`kind_stated` is now false on all but 9 rows, and on *every* `Bridge` row.**
  L9's `java/lang/String` migration is those 9. All 10,842 `Bridge` rows still
  inherit their kind.

The gate's own logic is exercised hermetically in the **blocking**
`jdk-only-blockers-selftest` CI job — 14 checks including an injected
unadjudicated `Bridge` it must reject and an adjudicated one it must accept, so
it is shown to fail and shown not to be always-red on every run.
