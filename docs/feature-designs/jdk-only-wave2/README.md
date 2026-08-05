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
| [L4](L4-overlay-detector-blind-spots.md) | Detector misses reads, same-kind writes, null writes | `vm/src/vm/vm_exec.rs` (hunter only) | — | M |
| [L5](L5-nativekind-native-io.md) | `register_with_kind` migration, `native-io` first | `native-io/src/*.rs` | — | M |
| [L6](L6-unadjudicated-bridge-ratchet.md) | Ratchet the 10,084 unadjudicated `Bridge` rows | `native-builtins/tests/`, `scripts/` | — | S |
| [L7](L7-ensure-synthetic-class-migration.md) | Make fabrication refusable, migrate the 3 live callers | `classloading/src/class_manager.rs` + callers | — | M |
| [L8](L8-strict-corpus-green.md) | Criterion 6: strict corpus green | `probes/`, `regression-suite/` | — | L |
| [L9](L9-blocker-rkc16n6-string.md) | **Blocker.** Real `String` bytecode during JDK `<clinit>` | `vm/src/runtime/interpreter/` | — | L |
| [L10](L10-blocker-threadpool-init.md) | **Blocker.** Real `ThreadPoolExecutor` field init | `native-collections/src/lib.rs` ⚠ | — | L |
| [L11](L11-delete-the-hardcoded-lists.md) | Items 3 + 7: delete the lists | `native_override.rs`, `vm_exec.rs` ⚠ | **L9, L10** | M |
| [L12](L12-item11-residuals.md) | Item 11 §2/§4/§6/§8/§9/§10/§11 | mixed — see doc | partly L5 | L |

**L3–L9 can all start today, in parallel, by different people.** (L1 and L2 are
done.)

## Conflict matrix — read before claiming a second lane

Two lanes are safe together iff they own disjoint files. The collisions that
exist:

| Pair | Collides on | Resolution |
|---|---|---|
| L2 ↔ L10 | `native-collections/src/lib.rs` | **Resolved:** L2 landed 2026-08-04; L10 rebases onto it. |
| L4 ↔ L11 | `vm/src/vm/vm_exec.rs` | L4 owns the overlay hunter (~line 3070–3200); L11 owns dispatch (~14700, ~22700). Disjoint regions in one file — coordinate, do not both `git add -A`. |
| L3 ↔ L12 | `lang_invoke.rs` | L3 is a handful of lines; land it first. |
| L5/L6/L12 ↔ each other | `register_with_kind` semantics | Only L5 changes call sites; L6 only reads the census; L12 §4 is JIT-side. Safe. |

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
python3 scripts/jdk-only-adjudicate.py c.json
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
  ClassLoader there). `overlay_write_is_destructive` only flags cross-type-class
  coercions, so zero of it appeared in any census.
  `classloader_parent` was returning the platform loader's own name String as
  its parent, and nothing measured it until a behavioural probe was diffed
  against the host JDK. **Read the writer against `javap` of the real class;
  the census is a floor, and for reference-into-reference it is a floor of
  zero.**

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

Two things that block a clean read of item 2's remaining rows, both L4's:
a null written over a primitive is still invisible to the hunter, and a
same-kind wrong-slot write always was. **Every count in item 2 is a floor.**

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
