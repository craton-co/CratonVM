# The 8,977 `Bridge` registrations the image does not back — contract §8's wave — RETIRED 2026-08-11

**Retired because all four items on its own "What would close this" list are
done, and the population it existed to hold is now gated rather than narrated.**

The record's purpose was explicit: two predecessors had been retired with their
own items closed, and "handing the remainder off by falling silent is how a
measured population becomes folklore", so the number was re-homed here. It is
no longer a number in a document — every part of it is a slack-free ratchet with
a committed baseline, scored by `regression-suite/bridge-ratchet.sh` on a real
JDK image.

Measured on Azure linux, JDK 25.0.4+7, 2026-08-11.

---

## Item 1 — resolve `image_declaring_method` up the hierarchy. **DONE.**

`ClassManager::adjudicate_natives_against_image` asked the image about ONE class
name and stopped, so a triple declared `ACC_NATIVE` on a supertype came back
`declared: false` and was counted as unadjudicated. The record put the cost at
"every reading of the ratchet is 19 too high" and required a HotSpot run plus
`scripts/jdk-only-inherited-decl.sh` to see past it.

It now walks the superclass chain and then superinterfaces, in JVMS §5.4.3.3
order — which is also CratonVM's receiver-driven dispatch order, so the answer
is a statement about what would actually run — bounded by a depth cap and a
visited set. `ImageMethodVerdict` gained `inherited_from`,
`inherited_acc_native`, `inherited_has_code` and `inherited_abstract`; the
census went to **schema 4** and `jdk-only-bridge-ratchet.py` REFUSES schema 3
rather than degrading, because scoring the older shape with the new arithmetic
is wrong in the "more work outstanding" direction.

What the record predicted, and what the walk measured:

| | record | measured |
|---|---:|---:|
| inherit an `ACC_NATIVE` supertype method | 19 | **19** |
| inherit concrete bytecode (§1.4 shadows) | ~1,600 | **1,613** |
| inherit an abstract method | ~300 | **283** |
| genuinely nowhere in the hierarchy | "the rest" | **363** |

The `undecl` bucket was 2,278. It is 363. `jdk-only-inherited-decl.sh` and its
HotSpot run are no longer on the path — `jdk-only-adjudicate.py` reads the
census's own columns and says `hierarchy-resolved: True` so a reader can never
mistake a schema-3 file's numbers for these.

**One thing the record did not anticipate.** A naive hierarchy walk credits four
more than 19: `java/lang/reflect/{GenericArrayType, ParameterizedType,
TypeVariable, WildcardType}.hashCode()I` resolve to `java/lang/Object.hashCode`,
which IS `ACC_NATIVE`. The resolution is factually correct — JVMS §5.4.3.3
reaches Object's public methods for an interface too — but it is not an
adjudication: `Object` declares `hashCode`, `clone`, `getClass`, `notify`,
`notifyAll` and `wait` native and EVERYTHING inherits them, so crediting it
would let any `X.hashCode()I` Bridge on any class discharge its §1.5 claim by
pointing at a method every object has. The fact stays in the census; the refusal
to credit it is a gate policy, stated once in
`jdk-only-bridge-ratchet.py::_inherits_from_object` and shared by both readers.
With it, the credit is exactly the 19 the record named.

## Item 2 — pick a subsystem and retire its shadows. **DONE: `java.util.logging`.**

The record prescribed "arm `CRATONVM_ENFORCE_NATIVE_SHADOW=1`, take the strict
corpus, fix what the class state needs, then re-tag and re-take the census", and
said the dial "exists so the measurement can be re-taken one subsystem at a time
instead of argued about".

**The dial could not do that.** It was a boolean. The only measurement it could
produce was the whole-VM one — 3 passed / 46 failed — which is an answer about
`java.base`'s object model and says nothing about any individual family. The
mechanism the item assumed did not exist.

`CRATONVM_ENFORCE_NATIVE_SHADOW` now also takes a comma-separated list of
internal class-name prefixes (`1`/`all`/`true`/`yes`/`on` and `0`/empty keep
their meanings; a dotted spelling normalises). Dispatch asks the **scoped**
predicate, `jdk_only_enforce_shadow_for(class_name)` — asking the global "is
anything armed" at the dispatch site would enforce one subsystem's dial across
the whole VM, which is the 3/46 collapse the scoping exists to avoid. A prefix
list that parses to nothing reads as OFF, so a typo cannot produce an inert run
that looks like a clean result.

The measurement, one binary, one workload, only the dial differing:

```
SUITE=jdk-only CRATONVM_ARGS=--jdk-only bash regression-suite/run.sh
  baseline                                            23 passed / 4 failed
  CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/logging/   23 passed / 4 failed
  CRATONVM_ENFORCE_NATIVE_SHADOW=javax/management/    23 passed / 4 failed
failing set unchanged throughout: RJdkHandles RJdkReflect RJdkForkJoin RJdkJmx
```

Verdict-neutral is the criterion, not green: those four fail on `dev` for
reasons unrelated to either subsystem, and a change that left them failing for a
NEW reason would be a regression this comparison catches. `javax/management/`
does exactly that — the set is unchanged but `RJdkJmx`, already red, changes its
failure face — so JMX is **not** a clean subject yet and is left for the next
lane. `java/util/logging/` is clean end to end.

The 84 live triples that measurement covers are re-tagged `SyntheticStub` at
registration via `native-api/src/retired_shadow.rs`, a sibling of
`no_image_receiver` and applied in the same arm for the same reason: the
property is a measurement against an image that no registration site can know,
and the sites do not reliably name themselves anyway — `registered_by` is a
`#[track_caller]` record, so a shared `with_category` helper attributes a
`LogRecord` triple to `native-io/src/nio_native.rs`. Four different files
register these 84.

Acceptance with the retirement live: strict corpus **23/4, same set**; core
corpus **37/0**. The ratchets moved by exactly the rows retired:
`without_acc_native` 9,015 → **8,911**, `shadows_bytecode` 6,170 → **6,066**.

**One `java/util/logging/` bridge is held back by name**, and it is why the list
is per-TRIPLE rather than a class prefix. `Logger.log` has eight registered
overloads; seven shadow real bytecode. The eighth is
`log(Level, Supplier, Throwable)` — not a JDK 25 signature at all, the real
overload takes the `Throwable` second — so the census resolves it nowhere and
refusing it would replace a shadow with an `UnsatisfiedLinkError`. That is the
shape the 2026-08-10 wave hit when four of 43 re-tagged receivers had to be held
back, and the record's own "expect that ratio" note was right.

## Item 3 — probe the abstract rows outside `java.util`. **DONE, and measured INERT.**

`regression-suite/src/RForeignLayoutJdkInterfaces.java` hands the VM a
`java.lang.reflect.Proxy` — the most foreign implementor there is, with no
fields at all and every method routed to an `InvocationHandler`, and also the
realistic one, since JDBC pools, Mockito, Spring AOP and the JDK's own tracing
wrappers hand out proxies over exactly these interfaces.

Eleven families, chosen as the largest live abstract-target populations that an
application actually implements: `java.sql` (`Connection`, `ResultSet`,
`ResultSetMetaData`, `Statement`, `PreparedStatement`, `DatabaseMetaData`),
`java.nio.file.Path`, `java.lang.ProcessHandle`, `javax.net.ssl.SSLSession`,
`java.lang.management.{ThreadMXBean, RuntimeMXBean}`,
`javax.management.MBeanServer`, `javax.xml.stream.XMLStreamReader` and
`java.nio.file.attribute.DosFileAttributes`.

Every call asserts BOTH the returned sentinel and that the handler recorded the
invocation, so interception is visible whether it fabricates a value or merely
skips the handler. **146 checks, byte-identical to HotSpot in `--real-jdk` and
`--jdk-only`**, with the real `Path`, `ProcessHandle` and MXBeans asserted
unaffected in the same run.

The abstract CLASSES left in that population — `java.nio.ByteBuffer`,
`java.nio.channels.SocketChannel`, `java.net.http.HttpClient`,
`java.lang.foreign.MemorySegment` — cannot take a foreign implementor at all
(package-private constructors, sealed types). That is a fact about the type, not
an untested gap, and the vector's header records it so nobody re-opens it.

## Item 4 — the 967 superseded rows. **CLOSED, and NOT by deletion.**

The record called them "deletable on their own evidence" and expected the
deletion to move the ratchet population to 8,010 "without deciding anything".
Measuring them first is what changed the disposition:

* **1,215** superseded registrations; **1,163 state the SAME kind as the
  winner**, so deleting them decides nothing — which is the record's own point,
  turned around.
* Roughly **430** are one registrar called more than once. The tree already
  documents that as deliberate and idempotent (`register_stamped_lock_natives`
  is called three times per boot and says so at length); deleting those means
  removing a CALL, not a registration.
* Which row is the loser is a function of **call order in `vm_init`**, so
  "delete the loser" bakes call order into the source.

Bulk deletion is therefore refused, with that evidence recorded rather than the
verdict asserted.

What IS actionable is the **52 rows whose kind DISAGREES with the winner's**.
There the kind that ships was chosen by call order rather than by anyone, and
kind drives three policies (`--jdk-only` refuses a `SyntheticStub`,
`CRATONVM_NO_STUBS` drops one, and only a `SyntheticStub` is subject to the
yield arbitration). **Four of the 52 are the dangerous direction**: a registrar
tagged the triple `SyntheticStub` and a later one ships it as a `Bridge`, so
`--jdk-only` ADMITS a registration somebody classified as a fake. Both counts
are now ratchets, with selftest injections including the proof that an AGREEING
superseded pair fires nothing.

## The 24, and the gate that was missing

The record called out 24 registrations / 12 triples on `ForkJoinTask`,
`RecursiveTask` and `RecursiveAction` that state `Bridge` over concrete bytecode
on all six images — "a §1.4 shadow wearing a §1.5 claim" — and noted precisely
how they survived: L6's ratchet counts rows without an `ACC_NATIVE` target,
`jdk-only-kind-map.py` freezes each row's kind, and **neither asks whether a
`kind_stated` row's claim is TRUE**, while `kind_stated` is the column every
reader treats as "somebody checked this against the image".
`jdk-only-adjudicate.py` §3b printed it, and a print is not a gate.

The registrations stand — they are deliberate, load-bearing shadows (the site
comments record that `RJdkForkJoin` hangs without them) and they belong to the
shadow population and its blocker, exactly as the record says. The STATEMENT is
now gated: `bridge_stated_shadows_bytecode`, frozen at **24**, with selftest
cases for a stated `Bridge` over declared and over inherited bytecode. A 25th
cannot arrive unnoticed.

## The gates this leaves behind

`regression-suite/bridge-ratchet.sh`, one census, five slack-free ratchets plus
the kind map, all keyed by `<jdk-feature>/<os>`:

| ratchet | baseline |
|---|---:|
| `bridge_without_acc_native` (hierarchy-wide) | 8,911 |
| `bridge_shadows_bytecode` (declared **or inherited**) | 6,066 |
| `bridge_stated_shadows_bytecode` | 24 |
| `superseded_kind_disagreements` | 52 |
| `superseded_stub_lost_to_admitted` | 4 |

The gate's own hermetic self-test grew from 13 checks to 28, every new ratchet
shown failing on an injection before it was frozen. A guard never shown to fail
is decoration.

**What remains open is the shadow population itself** — 6,066 rows — and it is
open for the reason the record already established and this lane re-confirmed:
the class's state has to become real before its shadow can be retired. What has
changed is that the retirement now has a mechanism, a per-subsystem measurement
procedure and a worked example, instead of an instruction. The next lane picks
`javax/management/` (and starts by finding out why `RJdkJmx` fails at baseline).

---

The original record follows unchanged.

---

# The 8,977 `Bridge` registrations the image does not back — contract §8's wave

**Status:** OPEN, filed 2026-08-10 as the surviving owner of a question two
retired records used to hold. Nothing here is a crash. What is open is that
8,977 registrations are tagged `Bridge` while no supported JDK image declares
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
counts   intrinsic 663   bridge 9757   synthetic-stub 1135   total 11555

kind               rows  absent  undecl  native    code  abstract
bridge             9757     872    2232     780    4555      1318

BRIDGE rows with no ACC_NATIVE target:            8977
  ...inherited an ambient set_category:           8890
  ...dispatched this run:                            2
  ...superseded (own no slot, can never dispatch): 967
  => LIVE unadjudicated BRIDGE surface:           8010
```

**Read `8977` as L6's ratchet population and `8010` as the work.** The gap is
registrations a later `register*` of the identical triple displaced: they record
that a registration happened and nothing more, and adjudicating their kind
decides nothing. `owns_slot` is a census column, so this no longer has to be
inferred from row order.

`--inherited` splits the 2,232 `undecl` rows, which is the difference between a
work list and a four-times-too-large one. Roughly: ~1,600 inherit concrete
bytecode (§1.4 shadows), ~300 inherit an abstract method (they intercept every
implementor), **19 inherit an `ACC_NATIVE` supertype method** — bridges the
census does not credit — and the rest are genuinely nowhere in the hierarchy.

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
the 4,555 shadow population and its blocker rather than to a quick fix.

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
| 1,055 | `native-builtins/src/lib.rs` |
| 990 | `native-builtins/src/lang_misc.rs` |
| 950 | `native-collections/src/lib.rs` |
| 394 | `native-builtins/src/phases_late/nio_file.rs` |
| 363 | `native-builtins/src/phases_late/foreign_ffm.rs` |
| 264 | `native-io/src/lib.rs` |
| 263 | `native-builtins/src/net_phase_e.rs` |
| 239 | `native-builtins/src/util_concurrent_ext.rs` |
| 215 | `native-builtins/src/servlet.rs` |

`native-collections`' 950 come from **one** `set_category(Bridge)` line, and the
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
  248 such rows are `SyntheticStub`, by measurement, in
  `native-api/src/no_image_receiver.rs`. The `class_absent` column's remaining
  872 is third-party names an application supplies, plus the proxy machinery
  kept `Bridge` as a reviewed VM service. Do not re-open it as part of this wave.
* **The abstract-interception worry is measured inert for `java.util`.** A probe
  handing the VM `AbstractCollection`/`AbstractSet`/`AbstractList`/`AbstractMap`
  subclasses in a layout nothing models gets byte-identical answers to HotSpot on
  all 42 observables — the retired
  `abstract-collection-natives-are-inert-for-foreign-layouts` record, whose probe
  is now the scheduled vector `regression-suite/src/RForeignLayoutCollections.java`.
  The other 1,318 abstract rows are unprobed.

## The blocker, measured rather than argued

The obvious disposition for the 4,555 shadows is `SyntheticStub`: strict mode
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

The same blocker had a smaller, already-worked example, and watching it close is
the useful part: of the 43 receiver classes re-tagged on 2026-08-10, four had to
be held back because strict mode still fabricated them, and dropping their
natives replaced a silent §5 violation with `UnsatisfiedLinkError`. Two of the
four were caught by the corpus; the other two were latent and came from a
class-origin census — expect that ratio. All four were released hours later when
`ensure_synthetic_class` was deleted, which is what "the class's state has to
become real first" looks like when it actually happens.

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
4. **The 967 superseded rows are a separate, cheaper job**: they can never
   dispatch, so they are deletable on their own evidence, and removing them
   would take the ratchet population to 8,010 without deciding anything.

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
