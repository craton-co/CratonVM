# H0-5 — the 22 `HashMap` failures are not one defect, and the largest is a view with no outer

**Status: OPEN — MEASURED.** Every row below is a run on
`C:/craton/target-jdkonly-h2/release/cratonvm.exe` at `85b6d84ac`, whose
unarmed strict arm is **104 / 104**. No source change.

Lane H0 (orchestrator), 2026-08-20. Answers `H0-3` §5 N2 and extends `H0-4`.

---

## 1. The question

`H0-4` measured `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap` taking the
strict arm to **81 / 104** and left the obvious question open: *is that one
defect with 22 faces, or several?* `H0-4` §4 had already shown that the row
common to four families (`RMapGcStress`) **is** one defect with four faces, so
the same answer was plausible here.

**It is not.** There are at least two dominant mechanisms and they want different
fixes.

## 2. The control, which changes what two of the rows mean

Each vector run **bare** (no `run.sh`) twice — once unarmed, once with the
prefix armed. `1` means the run ended in `main-vm run() returned Err`.

| vector | unarmed | armed |
|---|---:|---:|
| `RCollections` `RSerial` `RExecutorShutdown` `RJdkViews` `RJdkCollections` `RJdkSecurity` `RBlockingQueue` `RChannelInterrupt` `RFileTimes` `RSimpleTimeZoneRaw` `RJdkOptionalShape` `RJdkStrict` `RJdkExecutors` `RJdkForkJoin` `RJdkNio` `RJdkProcess` `RJdkJmx` `RJdkFailure` `RJdkEnvMap` `RJdkEnumerations` | **0** | **1** |
| `RJdkModule` | **1** | 1 |
| `RJdkLogging` | **0** | **0** |

**Twenty of the twenty-two are caused by the dial**, cleanly: they pass bare
without it and fail bare with it.

**Two are not diagnosable this way, and I nearly recorded a wrong cause for one.**

* `RJdkModule` fails bare **whether or not the dial is armed** — its bare
  failure is `cratonvm.jdkonly.svc is not in the boot layer -- was
  --module-path/--add-modules passed?`, i.e. the missing per-vector launch args
  that `run.sh` supplies. Its failure *under the arm* is real; my bare
  reproduction of it is not evidence about the dial at all. `HANDOFF-20260819`
  §8 already warns that **bare vector invocations are not the arm**, and
  `RJdkForeign`/`RJdkModule` are the two it names.
* `RJdkLogging` passes bare in **both** configurations while failing under the
  arm, so whatever the dial does to it depends on the launch args too.

Both need `run.sh`'s per-vector arguments to diagnose. Not done here.

## 3. Mechanism A — a view carrier whose outer reference was never set

Four vectors die with the identical JVM-generated message:

```text
java/lang/NullPointerException:
  Cannot read field "modCount" because "this.this$0" is null
```

`RExecutorShutdown`, `RJdkViews`, `RJdkSecurity`, `RBlockingQueue`.

`this$0` is the **synthetic outer reference** javac gives a non-static inner
class. `HashMap`'s views and iterators — `KeySet`, `EntrySet`, `Values`,
`HashIterator` — are all inner classes of `HashMap`, and each reads
`this$0.modCount` for its fail-fast check. So: **the VM hands out a view object
whose outer `HashMap` was never linked.** The view exists, its class is right,
and it has no map.

This is a *different* defect from the one `H0-4` §4 caught. There the container
was real and empty (`iterated 1 != 3000`); here the view is not connected to a
container at all. Both are consequences of the VM owning state that real
bytecode expects to find in fields, but the repair is not the same: one needs
the `table` populated, the other needs `this$0` written when the view is minted.

### CORRECTION, same day — this is confirmation, not discovery

I first wrote that this was *"the single most actionable finding in the
blast-radius work so far"* and that the VM had simply failed to link the view.
**Then I grepped for `this$0` and found the tree already documents it, in
detail, in three places** — including a note lane `H4-1` added earlier the same
day:

> *"A carrier is minted by a LIVE native with its list state at undeclared slots
> and `this$0` left null. **Refuse this registrar's rows for a carrier whose
> producer is still `Bridge` and the real JDK body runs instead —
> `TreeMap$Values.size()` dereferences that null `this$0`.** So the split has to
> be per carrier family, keyed on whether the producing registrar moved, not per
> registrar."*
> — `native-collections/src/lib.rs`, above `register_map_view_carrier_natives`

and, from `G22-1` N2 (**MEASURED**, and older):

> *"the reason CratonVM reported `HashMap$Values.this$0 == null` where HotSpot
> reports the backing map"*

So `this$0` being null on a view carrier is **known, deliberate and contained**:
the registrar's rows exist precisely to stop real JDK bodies reading that null,
and `force_native_over_real_jdk_bytecode` is paired with it. **Arming the dial
removes the containment**, the real bodies run, and they hit exactly what the
comment says they will.

**What my measurement adds is smaller than I claimed, and still worth having:**
it is the first *run* evidence of the predicted failure rather than a source
argument; it puts vector names against it; and §6 measures which families it
actually reaches, which no record had. **What it is not is a missing line
somebody forgot.** The repair is the migration itself — write a real `this$0`
when the carrier is minted, which means the producing registrar has to move
first, which is the constraint `H4-1` already stated.

I am leaving the wrong sentence quoted above rather than deleting it, because
the error is this directory's standing one and I made it while writing a record
about other people making it: **I asserted a cause without checking whether the
tree already knew.** One `grep this$0` would have cost thirty seconds.

## 4. Mechanism B — a fabricated class reaching a real array store

Four vectors die naming the same fabricated receiver:

```text
java/lang/ArrayStoreException: cratonvm.synthetic.AnonymousObject$4
java/lang/ClassCastException: class cratonvm.synthetic.AnonymousObject$4 cannot be cast to ...
```

`RJdkCollections`, `RSimpleTimeZoneRaw` (wrapped in an `Error`), `RJdkProcess`,
`RJdkFailure`.

A VM-minted `cratonvm.synthetic.AnonymousObject$4` is escaping into real code
that then stores it into a typed array or casts it. Note this is `GC guard`
territory adjacent to `W7-84` and to the `aastore` family (`W8-E6-1`,
`W8-E11-1`, `W8-E24-1`), and that **the same fabricated class name appears in
all four** — one producer, four consumers, most likely.

## 5. The remaining twelve

Not clustered, and I am not going to invent a taxonomy for them:

`RCollections` *equals foreign partial* · `RSerial` *HashMap round-trip* ·
`RChannelInterrupt` `NonWritableChannelException` · `RFileTimes`
`ExceptionInInitializerError` · `RJdkOptionalShape` *No group with name
`<VNUM>`* · `RJdkStrict` *identity as a toMap key mapper* · `RJdkExecutors`
*tpe terminated* · `RJdkForkJoin` NPE in `ReduceOps$AccumulatingSink` ·
`RJdkNio` `NoSuchFileException` · `RJdkJmx` `NotCompliantMBeanException` ·
`RJdkEnvMap` NPE on `this.i` · `RJdkEnumerations` *Properties own key count: 0*.

Several of these are plausibly downstream of A or B — `RJdkEnvMap`'s null
iterator field and `RJdkEnumerations`' zero key count both smell like A — but
**plausible is not measured** and this directory has been burned by exactly that
step. Each is one bare run and one stack trace away from an answer.

## 6. What this means for the migration order

`H0-4` §3 put `HashMap` last on cost. That still holds. But **mechanism A is not
`HashMap`-specific** — `this$0` on an inner view class is how *every* collection
in `java.util` implements its views, so the same defect is very likely under
`LinkedHashMap`, `TreeMap` and `Hashtable` too, and would be fixed once for all
of them.

That is worth checking before sequencing any migration: if A is a single write
missing at a single mint site, it may be cheap and may re-price several rows at
once — the same shape as `RMapGcStress` in `H0-4` §4, where one defect was
inflating four cells.

### CHECKED, same day, measured — mechanism A spans three families

Four vectors grepped for `this$0" is null` under each of three further prefixes:

| armed prefix | vector showing mechanism A |
|---|---|
| `java/util/HashMap` | `RExecutorShutdown`, `RJdkViews`, `RJdkSecurity`, `RBlockingQueue` |
| `java/util/LinkedHashMap` | **`RJdkMapViews`** |
| `java/util/Hashtable` | **`RJdkEnumerations`** |
| `java/util/TreeMap` | none of the four tested |

**So A is not `HashMap`-specific**, and the two it reaches are load-bearing
cells: `RJdkMapViews` is one of `LinkedHashMap`'s seven and `RJdkEnumerations`
is one of `Hashtable`'s three — which, net of the shared `RMapGcStress` row,
is **one of `Hashtable`'s remaining two**. A single missing `this$0` write would
therefore move cells in at least three rows of `H0-4`'s table.

**And `TreeMap` is a different problem.** Three of the four vectors tested
(`RJdkViews`, `RCollections`, `RJdkEnumerations`) are in `TreeMap`'s own failing
set and **none** of them shows `this$0`. Whatever breaks `TreeMap` is not
mechanism A, so `TreeMap` does not ride along on this fix. Stated because the
tempting inference — "views are inner classes everywhere, so this is universal"
— is exactly the kind this directory keeps recording as false.

*Scope: four vectors per prefix, chosen because they are view/enumeration
shaped. This is a targeted grep, not a census; a family could show A in a vector
I did not test.*

## 7. NOMINATIONS

* **N1 — ANSWERED by grep, see §3's correction.** `this$0` is deliberately left
  null and the registrar's rows exist to contain it. The real nomination is
  `H4-1`'s: **the split must be per carrier family, keyed on whether the
  PRODUCING registrar moved** — `MAP_VIEW_CARRIERS` spans four ownership
  families, so retagging that one registrar is a partial retag of three of them.
* **N2 — find the single producer of `cratonvm.synthetic.AnonymousObject$4`.**
  Four consumers name it; one grep should name the producer.
* **N3 — DONE, see §6.** A reaches `LinkedHashMap` and `Hashtable` but not
  `TreeMap`. Fix it before any per-family migration: it moves cells in three
  rows at once.
* **N4 — diagnose `RJdkModule` and `RJdkLogging` under `run.sh`'s own launch
  args**, since bare runs cannot see them. `run.sh` supplies `class_cv_args`
  and module-path flags per vector.
