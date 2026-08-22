# H14-3 — five of the thirteen retirements nobody proposed cost NOTHING, and `java/util/Properties` is worse than `HashMap`

**Status: OPEN — MEASURED, thirteen full 104-vector strict arms plus one
unarmed baseline and a three-way control.** All on
`C:/craton/target-jdkonly-h2/release/cratonvm.exe` (built at `fe59bf9d9`),
whose **unarmed baseline in the same session is 104 / 104** with `saturation:
none`. No source change; no build.

Lane H14, 2026-08-20/21. Extends `H0-4`'s blast-radius table from six
collection prefixes to thirteen registrars, chosen from the measured
distribution in `H14-1`/`H14-2` rather than from examples. **Read
`H14-2` §5 first: nineteen of the top twenty-five registrars in the shadow
population had never been named as retirement candidates anywhere.**

---

## 1. The table

`CRATONVM_ENFORCE_NATIVE_SHADOW=<prefix-list>` makes contract §1.4 **enforced**
rather than counted for those prefixes, which is what a permanent retirement
does — so this prices a retirement before anyone attempts it, one env var and no
build. Each registrar is armed on **its own classes**, taken from the CSV
`shadow-triage.py` emits, so a row prices *that registrar*, not a family.

| # | registrar (from `H14-2` §5) | rows | armed prefixes | **passed / 104** | failed | **net of the timeout artefact** (§3) |
|---:|---|---:|---|---:|---:|---:|
| 1 | `lang_misc.rs::register_throwable_subclass_natives` | 42 | 26 exception classes | **104** | 0 | **0** |
| 2 | `lang_string.rs::register_string_builder_natives` | 57 | `StringBuilder`, `StringBuffer` | 103 | 1 | **0** |
| 3 | `native-collections::register_array_deque_natives` | 31 | `java/util/ArrayDeque` | 103 | 1 | **0** |
| 4 | `native-collections::register_optional_natives` | 20 | `java/util/Optional` | 103 | 1 | **0** |
| 5 | `phases_late.rs::register_p64_hex_format` | 24 | `java/util/HexFormat` | 103 | 1 | **0** |
| 6 | `native-io::register_data_stream_natives` | 20 | `DataInputStream`, `DataOutputStream` | 102 | 2 | **1** — `RDataInputFastPull` |
| 7 | `native-io::register_io_natives` | 23 | `ByteArray{In,Out}putStream`, `FileOutputStream` | 102 | 2 | **1** — `RNioNoFollow` |
| 8 | `nio_file.rs::register_phase57_file` | 22 | `java/io/File` | 102 | 2 | **1** — `RNioNoFollow` |
| 9 | `net_phase_e.rs::register_uri_natives` | 24 | `java/net/URI` | 102 | 2 | **1** — `RJdkBridge1` |
| 10 | `phases_late.rs::register_p71_biginteger_extras` | 24 | `java/math/BigInteger` | 101 | 3 | **2** — `RJdkIntrinsics3` `RJdkSecurity` |
| 11 | `nio_file.rs::register_phase57_nio_file` | 34 | `Files`, `Path`, `Paths`, `FileSystemProvider`, `BufferedWriter`, `Arrays$ArrayList` | 98 | 6 | **5** — `RCrypto` `RFileTimes` `RNioNoFollow` `RForeignLayoutJdkInterfaces` `RJdkNio` |
| 12 | `properties_sidetable.rs::register_properties_sidetable` | 20 | `java/util/Properties` | **65** | 39 | **38** |
| 13 | `lib.rs::register_essential_natives_with_shims` | 184 | its 41 classes, incl. `java/lang/Object` | **28** | 76 | **74** |

Every arm was checked for contamination at the moment it finished: exactly one
`REGRESSION SUITE` summary line and **zero `HARNESS ERROR [G4]`** in all
thirteen. §5 says why that check exists.

### Placed against `H0-4`'s six

| prefix | net cost | source |
|---|---:|---|
| `register_throwable_subclass_natives` (26 exception classes) | **0** | here |
| `java/util/HashSet` | 0 | `H0-4` |
| `StringBuilder+StringBuffer`, `ArrayDeque`, `Optional`, `HexFormat` | **0** | here |
| `DataStreams`, `ByteArrayStreams`, `java/io/File`, `java/net/URI` | **1** each | here |
| `java/util/Hashtable` | 2 | `H0-4` |
| `java/math/BigInteger` | **2** | here |
| `java/nio/file` group | **5** | here |
| `java/util/LinkedHashMap` | 6 | `H0-4` |
| `java/util/TreeMap` | 7 | `H0-4` |
| `java/util/concurrent/ConcurrentHashMap` | 10 | `H0-4` |
| `java/util/HashMap` | 22 | `H0-4` |
| **`java/util/Properties`** | **38** | **here** |
| **`register_essential_natives_with_shims`** | **74** | **here** |

## 2. The two results that change the plan

### 2a. Nine retirements are free or nearly free, and they clear 265 shadows

Rows 1–9 cost **zero to one vector each** and between them own
**263 of the 1402** (18.8%) — more than `H0-4`'s entire six-family table
(200 rows, 14.3%) at a fraction of its measured cost. The five zero-cost rows
alone are **174 rows** (12.4%).

Two of them are the second and third largest registrars in the tree
(`register_string_builder_natives` 57, `register_throwable_subclass_natives`
42), and **neither has ever been proposed for retirement**.

`register_throwable_subclass_natives` is the strongest single result here:
**42 rows over 26 exception classes and a completely clean 104 / 104**, with no
failure of any kind, not even the timeout artefact.

### 2b. `java/util/Properties` is the most expensive prefix ever measured — worse than `HashMap`

**65 / 104.** `H0-4` called `HashMap` (81/104, net 22) *"the floor"*. Properties
is a storey below it, and the failure list says why: `RJdkHello` — the simplest
vector in the corpus — fails, along with `RStrings`, `RSerial`, `RCrypto`,
`RSimpleDateFormatZone`, `RJdkModule`, `RJdkLogging`, `RJdkSecurity`,
`RServiceLoaderDoubleSource` and thirty more. `System.getProperty` funnels
through `Properties`, and the VM owns that side table
(`native-builtins/src/properties_sidetable.rs`), so enforcing §1.4 there takes
away the system property map the whole runtime is built on.

This is the same shape `H0-3` found under `ConcurrentHashMap` and `H0-4` under
`HashMap` — *"crypto provider chains, the logger registry, the module graph, the
proxy cache and service loading are all built on a map whose contents the VM
owns"* — one level lower again. **`G88-1` §6's Properties/Hashtable cluster is
not a mid-sized job. It is the deepest one measured.**

### 2c. The monolith is an upper bound, and it is 74

Arming all 41 classes `register_essential_natives_with_shims` registers
directly — `java/lang/Object`, `java/lang/Class`, `java/lang/System`,
`java/lang/Thread`, `jdk/internal/misc/Unsafe` among them — leaves **28 / 104**.
That is the honest price of "retire the largest bucket", and it is why
`H14-2` §6 calls that bucket a bucket rather than a work item. Subdivide by
class and re-price; the cell above is the ceiling, not the plan.

## 3. The shared row is a TIMEOUT, and it is not a dial effect at all

`RMapGcStress` failed in **12 of the 13 arms** — the same vector `H0-4` §4
found common to four of its six. It would be natural to net it out as one
defect with many faces, exactly as `H0-4` did.

**That reading would be wrong here, and the failure signature says so:** every
one of these is `cratonvm rc=124`, the 120 s `timeout`, where `H0-4`'s were
assertion failures with real messages (`iterated 1 != 3000`, `lost/wrong value
for 1 -> null`).

Control, MEASURED, one vector, `TIMEOUT=600`:

| arm | wall | verdict |
|---|---:|---|
| **unarmed** | **233 s** | **PASS** |
| `StringBuilder`+`StringBuffer` armed | 224 s | PASS |
| `java/util/HexFormat` armed | 249 s | PASS |

`RMapGcStress` needs about four minutes on this host and the suite gives it
two. It passes under every condition when given room, **including armed**, and
the arm moves the wall time by less than the run-to-run spread. It is a
timeout-marginal vector, not a shadow defect — and note it *passed* in the
unarmed 104/104 baseline earlier the same session, before a concurrent
workload from another lane raised the host's load (§5).

`RMapResizeGc` fails the same way (`rc=124`) in the monolith arm only, and is
netted out on the same grounds.

**So the "net" column of §1 nets out a HOST artefact, not a VM defect.** The two
must not be conflated:

* `H0-4` §4's `RMapGcStress` — assertion failures — **is** a real shared defect
  and its netting stands.
* This record's `RMapGcStress` — `rc=124` — is the clock.

One vector name, two entirely different stories, one arm apart. Read the
failure line, never the vector name.

## 4. What each non-artefact failure actually is

Not diagnosed here — each is a nomination — but the signatures are recorded so
the next lane does not re-measure them:

| arm | vector | signature |
|---|---|---|
| `data_stream` | `RDataInputFastPull` | `AssertionError` |
| `io_natives`, `phase57_file`, `phase57_nio_file` | `RNioNoFollow` | `AssertionError` |
| `uri` | `RJdkBridge1` | `rc=1` |
| `biginteger` | `RJdkIntrinsics3`, `RJdkSecurity` | `rc=1` |
| `phase57_nio_file` | `RCrypto`, `RFileTimes`, `RJdkNio` | `rc=1` |
| `phase57_nio_file` | `RForeignLayoutJdkInterfaces` | `AssertionError` |

`RNioNoFollow` appears under three different file-facing registrars, which is
the shape of a genuine shared row — the one worth diagnosing before any of the
three file retirements is attempted.

`register_phase57_nio_file` additionally carries the two rows `H14-1` §4 flagged
as **not retirable at all**: `java/nio/file/Path.toString()` and
`Path.equals(Object)` are declared on an interface with no `Code` attribute.
Arming the prefix is not the same as retiring the triples; a source retirement
of that registrar must exclude those two.

## 5. The measurement was contaminated once, and the way it showed is worth carrying forward

Three of these thirteen arms were run twice. The first run of each was
**discarded**, and had they been published they would have inverted the ranking:

| arm | contaminated run | clean re-run |
|---|---:|---:|
| `register_p64_hex_format` | 93 / 104, 27 failed — **23 `[G4]`** | **103 / 104, 1 failed** |
| `register_p71_biginteger_extras` | 48 harness errors, **19 `[G4]`** | **101 / 104, 3 failed** |
| `register_uri_natives` | 83 / 104 **and** 80 / 104, **18 `[G4]`** | **102 / 104, 2 failed** |

**The cause: a background sweep reported "killed" left its children running,**
so two sweeps drove `run.sh` at once over the same output files. The signatures,
in the order they became visible:

1. `HARNESS ERROR [G4] <vector>: the HotSpot oracle exited 0 but printed no
   'PASS' line` — **23 of them in one arm.** The armed prefix cannot affect
   HotSpot's own process; a G4 storm is always the host, never the dial. The
   unarmed baseline for this session has **zero** harness errors of any kind,
   which is what made the storm legible.
2. **Two `REGRESSION SUITE:` summary lines in one log** — the exact
   `W8-D2-1` signature, which that record documents and which was still the
   fastest way to see it.

The lesson is not "check afterwards", it is **make it impossible**: the sweep
script now takes an `mkdir` lock and refuses to start a second instance, and it
quarantines its own arm the moment the arm produces `summaries != 1` or any
`[G4]`. Every number in §1 comes from a run that passed both tests.

This is `HANDOFF-20260819` §8's load trap in its sharpest form yet recorded: the
load did not shift a timing, it moved a published cell by **twenty vectors**.

## 6. What this does NOT establish

* **A zero is permission to attempt and measure, not permission to skip
  measuring** (`H0-4` §4). Five zero-cost cells mean *these 104 vectors raise no
  objection*. The corpus has no AWT vector at all (`G79-1`), and `G90-1` §5 is
  the standing case of a narrow screen passing what a wider arm rejected.
* **The dial is not the retirement.** `CRATONVM_ENFORCE_NATIVE_SHADOW`
  suppresses **every** native on the armed class. For the **162 shadow triples
  with more than one registration** (`H14-1` §5), deleting one registrar's line
  exposes the *other* registration rather than real bytecode, so a source
  retirement is **weaker** than the dial and can measure as "no effect".
  Conversely, arming a prefix also suppresses natives belonging to registrars
  other than the one being priced — the cells above are per-**prefix-set**, and
  the attribution to a registrar is only as tight as the class list.
* **The prefixes are not disjoint.** Prefix matching is textual:
  `java/lang/Integer` also arms `java/lang/IntegerCache`, `java/lang/Module`
  also arms `java/lang/ModuleLayer`. Each cell was measured alone; **their sum
  is not the cost of arming several**, and nobody has run a combination.
* **Rows cleared is not rows fixed.** These are counts of §1.4 shadow
  observations, not of behavioural differences against HotSpot.
* **Compatible mode is untouched.** The dial only affects `--jdk-only`.

## 7. NOMINATIONS

* **N1 — land `register_throwable_subclass_natives` first.** 42 rows, 26
  classes, **104 / 104 with no failure at all**. It is the cheapest large
  retirement in the tree and it is not in any plan. Note `H14-2` §5's warning:
  8 of its 42 are registered on a class that does not declare the method, so the
  PR is *retire 34, relocate 8*, not *retire 42*.
* **N2 — then `register_string_builder_natives`** (57 rows, cost 0), the single
  largest single-registrar single-cluster item in the population.
* **N3 — then `ArrayDeque` (31), `HexFormat` (24), `Optional` (20)** — all cost
  0. With N1 and N2 that is **174 rows, 12.4% of the defect, for five PRs and a
  measured cost of zero vectors.** `ArrayDeque` needs the probe from
  `retired_shadow.rs`'s arm-C2 note re-run first: the corpus only touches a
  deque's ends and a probe that calls `delete(i)` turned C2 red in 2026-08-12.
* **N4 — re-price `java/util/Properties` as a P0-scale item.** At 65/104 it is
  the deepest dependency measured anywhere, deeper than `HashMap`. Anything
  scheduled behind it is scheduled behind the system property map.
* **N5 — diagnose `RNioNoFollow`**, which fails under three separate file
  registrars. One diagnosis unblocks rows 7, 8 and 11.
* **N6 — put this matrix in CI**, restating `H0-4` N3. It is now nineteen cells
  over two records, all reproducible with one env var each, and it exists only
  because two lanes remembered to type it.
* **N7 — the sweep harness belongs in the tree, not in a scratchpad.** The
  lock-plus-quarantine pattern in §5 is what makes a multi-hour arm sweep
  trustworthy on a shared host, and lane H10 owns the place it should live.
* **N8 — raise `TIMEOUT` for `RMapGcStress`, or split it.** It needs ~233 s
  unarmed against a 120 s budget (§3). Today it reads as a failure in twelve
  consecutive arms and cost this lane a re-measurement to disbelieve. It is also
  the vector `H0-4` §4 relies on for a *real* finding, so the false signal sits
  directly on top of a true one.

## 8. `INDEX.md` rows, written here because this lane may not write them

`docs/known-issues/jdk-only/INDEX.md` is outside lane H14's paths. Three rows,
in this directory's house style, for whoever merges:

```markdown
- [H14-1](H14-1-the-1402-shadows-are-149-registrars-and-none-were-adjudicated-20260820.md) — `OPEN` · **MEASURED.** The first classification of the `native-shadows-bytecode` population, ever. All **1402 / 1402** rows joined to a registrar (0 unattributed, all `owns_slot: true`) by a new probe, `regression-suite/probes/shadow-triage.py`, which does the (class, method, descriptor) → `registered_by` → enclosing top-level `fn` join. **149 registrars**; top 10 = 34.3%, top 25 = 55.8%. Three findings the report itself could not have shown: `kind_stated` is **`false` on all 1402** — not one native in the defect population was deliberately classified, they all inherit an ambient `set_category`; the image verdict splits the population into **1244 retire / 156 registered-on-the-wrong-class / 2 with no implementation at all** (`java/nio/file/Path.toString` and `.equals`, which a row-count-driven plan would delete); and **162 triples carry more than one registration**, of which only the `owns_slot` one is reachable. Also: `cluster-map.py`'s `fn` rule admits NESTED `fn`s and therefore put a local helper at the top of the ranking.
- [H14-2](H14-2-the-plan-is-aimed-at-fourteen-percent-of-the-defect-20260820.md) — `OPEN` · **MEASURED counts, ARGUED mapping.** `H0-4`'s six priced collection prefixes are **200 of 1402 rows — 14.3%**, and **135 of the 149 registrars have zero rows under any of them**. **445 rows (31.7%) are claimed by no P0/P1/P2 row at all**: `java.lang` core (168), `java.io` streams (99), `StringBuilder`/`StringBuffer` (57, and `java/lang/String` itself contributes **zero**), `java.lang.invoke` (56), `java.math` (24). 95 ownership clusters, 94 of them small and mostly one-registrar/one-class — the shadow population is far more separable than the registration population. Positive control: the `java/util/logging` and `sun/nio/fs` retirement waves show as **exact zeros**, while `java/util/ArrayList` still carries **38** rows after 12 triples were retired. Ranks the 25 largest registrars nobody has proposed retiring.
- [H14-3](H14-3-five-retirements-are-free-and-properties-is-worse-than-hashmap-20260821.md) — `OPEN` · **MEASURED, 13 arms + control.** Extends the blast-radius table from six family prefixes to thirteen registrars drawn from the measured distribution. **Five cost ZERO vectors** — `register_throwable_subclass_natives` at a clean **104/104**, plus `StringBuilder`/`StringBuffer`, `ArrayDeque`, `Optional`, `HexFormat` — **174 rows, 12.4% of the defect, for a measured cost of nothing**; four more cost one vector each (263 rows total). At the other end, **`java/util/Properties` is 65/104 — worse than `HashMap`'s 81** and the deepest dependency measured anywhere; `RJdkHello` fails. The 41-class `register_essential_natives_with_shims` arm is **28/104**, an upper bound rather than a plan. §3 corrects a trap: `RMapGcStress` fails in 12 of 13 arms with `rc=124`, and the control shows it needs **233 s unarmed** against a 120 s budget — a clock, not a defect, and NOT the same `RMapGcStress` finding `H0-4` §4 netted out. §5 records three arms discarded and re-run after two sweeps ran concurrently and moved a cell by twenty vectors.
```
