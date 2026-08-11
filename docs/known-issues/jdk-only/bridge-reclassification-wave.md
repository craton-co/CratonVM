# The 9,296 `Bridge` registrations the image does not back — contract §8's wave

**Status:** OPEN, filed 2026-08-10 as the surviving owner of a question two
retired records used to hold. Nothing here is a crash. What is open is that
9,296 registrations are tagged `Bridge` while no supported JDK image declares
their target `ACC_NATIVE`, so `--jdk-only` admits every one of them on a claim
nobody has checked.

This record exists because the two that carried the question —
`l5-native-io-bridge-residuals.md` and `l5bc-awt-builtins-bridge-residuals.md` —
were retired on 2026-08-10 with their own items closed. Handing the remainder
off by falling silent is how a measured population becomes folklore, so it is
re-homed here with its numbers rather than left to be re-derived.

Predecessor records, in the internal tree:
`retired/l5-native-io-bridge-residuals-RETIRED-20260810.md`,
`retired/l5bc-awt-builtins-bridge-residuals-RETIRED-20260810.md`,
`fixed-bugs/jdk-only-bridge-on-a-receiver-no-image-declares-FIXED-20260810.md`.

## The measurement

JDK 25.0.4+7 / linux, 2026-08-10, from a schema-3 census read with
`scripts/jdk-only-adjudicate.py --inherited`:

```
counts   intrinsic 665   bridge 10076   synthetic-stub 1133   total 11874

kind               rows  absent  undecl  native    code  abstract
bridge            10076     907    2492     780    4579      1318

BRIDGE rows with no ACC_NATIVE target:            9296
  ...inherited an ambient set_category:           9209
  ...dispatched this run:                            2
  ...superseded (own no slot, can never dispatch): 977
  => LIVE unadjudicated BRIDGE surface:           8319
```

**Read `9296` as L6's ratchet population and `8319` as the work.** The gap is
registrations a later `register*` of the identical triple displaced: they record
that a registration happened and nothing more, and adjudicating their kind
decides nothing. `owns_slot` is a census column, so this no longer has to be
inferred from row order.

`--inherited` splits the 2,492 `undecl` rows, which is the difference between a
work list and a four-times-too-large one:

| what the row actually resolves to | rows | what it is |
|---|---:|---|
| inherited, concrete bytecode | 1,599 | a §1.4 shadow |
| inherited, abstract | 316 | intercepts every implementor |
| inherited, `ACC_NATIVE` | 19 | **a bridge the census does not credit** |
| nowhere in the hierarchy | 602 | genuinely undeclared |

### The 19 that are miscounted in the dangerous direction

Eighteen `sun/nio/ch/FileDispatcherImpl.*` inheriting the syscall surface from
`UnixFileDispatcherImpl`, plus `java/awt/image/ComponentSampleModel.initIDs()V`
inheriting a native `initIDs` from `SampleModel`. All nineteen state their kind
already and are correct; the census scores them unadjudicated because
`image_declaring_method` resolves a triple against **one** class and stops.

Closing this properly means resolving up the hierarchy inside
`ClassManager::adjudicate_natives_against_image` rather than in a script that
needs HotSpot at analysis time. Until then every reading of the ratchet is 19
too high, and `jdk-only-adjudicate.py` prints them by name on every run so the
number is never quoted without them.

### And 24 in the other direction, which no gate was watching

Both retired records state that L5's criterion — a row may state `Bridge`
exactly when the image declares that triple `ACC_NATIVE` — "selects **zero**
rows tree-wide". Measured on 2026-08-10 it selects **87**, and sorting them took
three checks:

| | rows | verdict |
|---|---:|---|
| `ACC_NATIVE` on another of the six images | 59 | correctly stated; a one-image census cannot say so |
| inherits an `ACC_NATIVE` supertype method | 4 | correctly stated (`ComponentSampleModel.initIDs`, three `FileDispatcherImpl.*`) |
| **concrete bytecode on all six images** | **24** | **a §1.4 shadow wearing a §1.5 claim** |

The 24 are 12 triples, all in `native-builtins/src/phases_late/concurrent.rs`:
`ForkJoinTask.{fork, invokeAll ×3, quietlyComplete, quietlyInvoke, quietlyJoin
×2, quietlyJoinPoolInvokeAllTask, quietlyJoinUninterruptibly}` plus
`RecursiveTask.fork` and `RecursiveAction.fork`. They are deliberate,
load-bearing shadows — the site comments say `RJdkForkJoin` hangs without them —
so the wrong part is the *statement*, not the registration, and they belong to
the 4,579 shadow population and its blocker rather than to a quick fix.

They are called out because of how they survived: L6's ratchet counts `Bridge`
rows without an `ACC_NATIVE` target and refuses a **rise**;
`scripts/jdk-only-kind-map.py` freezes each row's kind and refuses a **change**.
Neither asks whether a `kind_stated` row's claim is *true*, and `kind_stated` is
the column every reader treats as "somebody checked this against the image".
`jdk-only-adjudicate.py` §3b prints it on every run now.

## Where it is

`=== 4. unadjudicated BRIDGE rows by registering file ===` from the same run.
Nine files hold half of it:

| rows | file |
|---:|---|
| 1,060 | `native-builtins/src/lib.rs` |
| 990 | `native-builtins/src/lang_misc.rs` |
| 953 | `native-collections/src/lib.rs` |
| 413 | `native-builtins/src/phases_late/nio_file.rs` |
| 363 | `native-builtins/src/phases_late/foreign_ffm.rs` |
| 320 | `native-builtins/src/lang_string.rs` |
| 269 | `native-io/src/lib.rs` |
| 261 | `native-builtins/src/net_phase_e.rs` |
| 239 | `native-builtins/src/util_concurrent_ext.rs` |

`native-collections`' 953 come from **one** `set_category(Bridge)` line, and the
image declares `ACC_NATIVE` on exactly zero of them — re-measured per row, not
inherited. That is the largest single-line blast radius in the tree and the
reason contract §8 scopes this subsystem-per-PR.

## What has already been settled, so nobody re-opens it

* **A statement lane has nothing left to do.** L5's criterion — a row may state
  `Bridge` exactly when the image declares that triple `ACC_NATIVE` — selects
  **zero** rows tree-wide, and has since 2026-08-06. Every remaining row is a
  reclassification, not a migration.
* **`ABSENT` on a platform-named class means "not measured here".** Six images
  are now swept (21.0.12+8 and 25.0.4+7 × linux, windows, macos) and
  `scripts/jdk-only-dead-sweep.py` refuses an image set that omits a platform.
* **A receiver class no supported image declares is already handled**, 2026-08-10:
  246 such rows are `SyntheticStub`, by measurement, in
  `native-api/src/no_image_receiver.rs`. That is the `class_absent` column's
  remaining 907 minus the third-party names an application supplies and the four
  receivers strict mode still fabricates. Do not re-open it as part of this wave.
* **The abstract-interception worry is measured inert for `java.util`.** A probe
  handing the VM `AbstractCollection`/`AbstractSet`/`AbstractList`/`AbstractMap`
  subclasses in a layout nothing models gets byte-identical answers to HotSpot on
  all 42 observables — see
  [`abstract-collection-natives-are-inert-for-foreign-layouts.md`](abstract-collection-natives-are-inert-for-foreign-layouts.md).
  The other 1,318 abstract rows are unprobed.

## The blocker, measured rather than argued

The obvious disposition for the 4,579 shadows is `SyntheticStub`: strict mode
drops them and the real bytecode runs, which is what §1.4 says should happen.
That was implemented as a dispatch-time dial and **measured**:
`CRATONVM_ENFORCE_NATIVE_SHADOW=1` takes the `--jdk-only` corpus from
**32 passed / 17 failed to 3 / 46**.

The failures are not dispatch faults. `System.props` is null, `Charset.forName`
hands out an instance of the abstract `java.nio.charset.Charset`,
`SharedSecrets.javaLangAccess` is null, `String`'s coder does not match its
`value[]`. Under `--jdk-only` the surviving bridges **are** the object model for
large parts of `java.base`, so yielding them to bytecode hands real code objects
it cannot service.

So the order is fixed, and it is not this record's choice: the class's state has
to become real before its shadow can be retired. That is wave-2 item 4
([`ensure-synthetic-class-cannot-enforce-only-record.md`](ensure-synthetic-class-cannot-enforce-only-record.md))
and the record of the dial's measurement,
`fixed-bugs/jdk-only-step1-bytecode-available-RESOLVED-20260806.md` in the
internal tree. The dial exists so the measurement can be re-taken **one
subsystem at a time** instead of argued about.

The same blocker has a smaller, already-worked example: of the 50 receiver
classes re-tagged on 2026-08-10, four had to be excluded because strict mode
still fabricates them, and dropping their natives replaced a silent §5 violation
with `UnsatisfiedLinkError`. Two of the four were caught by the corpus; the other
two were latent and were found by a class-origin census. Expect that ratio.

## What would close this

In order, and none of it is a codemod.

1. **Resolve `image_declaring_method` up the hierarchy** so the 19 inherited
   `ACC_NATIVE` rows stop being counted as unadjudicated, and the 1,599
   inherited shadows are visible without a second tool and a HotSpot run.
2. **Pick a subsystem and retire its shadows**, in the order the dial makes
   cheap: arm `CRATONVM_ENFORCE_NATIVE_SHADOW=1`, take the strict corpus, fix
   what the *class state* needs, then re-tag the registrations and re-take the
   census. `jmx.rs` (215) or `logmanager.rs` (99) are the size to start at;
   `native-collections`' 953 are the size to finish at.
3. **Probe the 1,318 abstract rows outside `java.util`** the way
   `UserImplementorInterceptProbe` probed the eleven inside it. The interception
   surface is real; whether dispatch uses it is a per-family question and has
   only been answered for one family.
4. **The 977 superseded rows are a separate, cheaper job**: they can never
   dispatch, so they are deletable on their own evidence, and removing them
   would take the ratchet population to 8,319 without deciding anything.

## Reproducing

```sh
cratonvm --real-jdk --java-home <JDK25> --explain-jdk-only \
    --dump-native-registry census.json -cp probes JdkOnlyCensusLoadProbe
JAVA_HOME=<same image> sh scripts/jdk-only-inherited-decl.sh census.json inh.tsv
python3 scripts/jdk-only-adjudicate.py census.json --inherited inh.tsv
```

`--explain-jdk-only` is not optional; without it `image_declaring_method` is
`null` for every row and the script refuses rather than scoring zeroes.
`regression-suite/bridge-ratchet.sh` scores the same census against
`scripts/baselines/jdk-only-bridge-ratchet.json` and is the gate that stops this
number rising.
