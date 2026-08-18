# G36-1 — the 77 reference-slot writes in `phases_early.rs`, and why none of them fires

**Status:** MEASURED — on a binary, three independent ways, against the oracle.
**Outcome: NO SOURCE CHANGE TAKEN, deliberately.** Not one of the 77 census
sites this lane owns is reachable under `--jdk-only`. Editing them would be
shipping a fix into dead code, which is the failure `8c72d23ca` recorded and
`G34-1` §5.1 warned about in the identical registrar family. The evidence that
they are dead is §2, §3 and §4; the reader who disagrees should start there.

**Provenance.** Binary: `C:/craton/target-rel3/release/cratonvm.exe`, built from
`9ae371468` (this branch's HEAD), mtime `2026-08-17 07:07`. This is the FIRST
binary in this directory that carries `G30-1`'s coercion instrument
(`a4ca60972`), so this record is the "after" `G30-1` §7 asked the next lane to
produce. Oracle: HotSpot 25.0.3+9-LTS at `$JAVA_HOME`. `C:/craton/target-fcheck/`
ignored (partly-failed build, lying timestamp). Vectors from
`C:/craton/cvm-mergecheck/regression-suite/build`. Probe written for this lane:
`G36Slots.java` (§3). Census: the tree's own `scratchpad/g30/census2.py`,
re-run unmodified.

**Files changed by this lane: none.** `git status --short` proof in §7.

---

## 0. The headline

| question | answer | evidence |
|---|---|---|
| how many census sites does this lane own? | **77**, all in `phases_early.rs`; `lang_system.rs` has **0** | census2.py, §1 |
| how many are group A (measured-live)? | **0** | §2 — a nine-vector instrument sweep, 2,321 coercion events, none with a frame in either file |
| how many fire when you deliberately drive every class they write? | **0**, and all 37 answers match HotSpot exactly | §3 |
| why? | the registrars that own them are called only from `#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides` — under `--jdk-only` they never run | §4, SOURCE-VERIFIED and confirmed by a 10,691-row `--dump-native-registry` in which their classes appear **zero** times |
| did anything new turn up? | **yes: `pointer-into-primitive` fires.** `G30-1` §4.1 said "no site in the census produces it and none fired in the sweep" | §5 — twice per `RCrypto` run, `jca/provider_chain.rs:317` |

`G30-1` ranked `phases_early.rs` the **top file** of the 400-site census at 77
sites. That ranking is correct as a count and misleading as a priority: the
census resolves a class by NAME at the allocation call and has no reachability
filter. **The registry dump is a cheap reachability filter and the next census
pass should apply it** (§6, N1).

---

## 1. The 77, as the census sees them

`scratchpad/g30/census2.py`, re-run unmodified on `9ae371468`: 400 sites, 77 in
`native-builtins/src/phases_early.rs`, **0** in
`native-builtins/src/lang_system.rs`. Of the 77, 39 are group C/E (the slot is
read back by some native in this tree) and 38 are group F.

| n | class#slot | real field : descriptor | species | group |
|---|---|---|---|---|
| 21 | `java/util/ArrayList#1` | `elementData : Object[]` | prim→ref | E |
| 7 | `java/util/EnumSet#1` | `ordinal : int` | null→prim | F |
| 7 | `java/math/RoundingMode#0` | `name : String` | prim→ref | E |
| 6 | `java/time/DayOfWeek#0` | `name : String` | prim→ref | F |
| 3 | `java/net/Socket#4` | `out : OutputStream` | prim→ref | F |
| 3 | `java/nio/ByteOrder#0` | `name : String` | prim→ref | F |
| 2 ea | `java/util/GregorianCalendar#0/#1/#2` | `fields:int[]`, `isSet:boolean[]`, `stamp:int[]` | prim→ref | E |
| 2 | `java/util/HashMap#1` | `values : Collection` | prim→ref | E |
| 2 ea | `ForkJoinPool#0`, `LocalDateTime#0/#1`, `ChronoUnit#0`, `Socket#2/#3`, `Signature#1` | | prim→ref | F |
| 1 | `java/util/HashMap#2` | `table : HashMap$Node[]` | prim→ref | **C — pinned, do not touch** |
| 1 | `java/util/HashMap$Node#2` | `value : V` | prim→ref | E |
| 1 | `java/net/URI#5` | `port : int` | null→prim | E |
| 1 ea | `KeyGenerator#1`, `Cipher#1`, `KeyStore#1/#2`, `GregorianCalendar#7` | | | F |

The real layouts are `javap -p` on Temurin 25.0.3+9, taken by the census
script, and spot-checked by hand here for the two largest clusters:
`java.math.RoundingMode extends java.lang.Enum`, so slot 0 is `Enum.name`
(`String`) and slot 1 `Enum.ordinal` (`int`); `java.util.ArrayList extends
AbstractList`, so slot 0 is `AbstractList.modCount` (`int`), slot 1
`elementData` (`Object[]`), slot 2 `size` (`int`).

**Read statically, these look bad.** `register_phase52_math_context` writes
`ctx.set_field(rm, 0, Value::Int(4))` into a freshly allocated
`java/math/RoundingMode` — the ordinal of `HALF_UP` — where the real slot 0 is
`name : String`; and `register_phase52_rounding_mode`'s `name`/`toString` read
it straight back with `ctx.get_field(this, 0).as_int().unwrap_or(0)`. If that
write coerced, every `RoundingMode` in the VM would read back `null` → `as_int()
== None` → `unwrap_or(0)` → **`UP`, for all eight constants**. That is the
prediction the rest of this record tests, and it is false.

---

## 2. MEASURED — the instrument, nine vectors, zero from this lane's files

`CRATONVM_DBG_COERCION=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 … --jdk-only`,
which removes `G30-1`'s rate limit and attaches a full backtrace to every
occurrence. The backtrace is what makes attribution possible at all: the live
collectors still report `class_id=-1 index=-1` (`G30-1` N1 is not landed), so
the *frame* is the only provenance there is.

| vector | exit | result | coercion log lines |
|---|---|---|---|
| `RJdkHello` | 0 | `PASS (41 checks)` | 66 |
| `RJdkLogging` | 0 | `PASS (79 checks)` | 426 |
| `RJdkExecutors` | 0 | `PASS (69 checks)` | 375 |
| `RStrings` | 0 | `PASS (46 checks)` | 207 |
| `RJdkNet` | 0 | `PASS (81 checks)` | 768 |
| `RCrypto` | 0 | `PASS (57 checks)` | 99 |
| `RSimpleTimeZoneRaw` | 0 | `PASS (393 checks)` | 54 |
| `RJdkCollections` | 0 | `PASS (69 checks)` | 117 |
| `RJdkIntrinsics3` | 0 | `PASS (1011 checks)` | 309 |

`RCrypto` is worth a line of its own: `G30-1` §2 recorded it `exit 1
(pre-existing)`. It is **green** on `9ae371468`.

Every event, attributed to the first `native_builtins` frame in its backtrace:

| n | species | access | desc | value | site |
|---|---|---|---|---|---|
| 465 | primitive-into-reference | unattributed | `L` | `Int(0)` | `reference.rs:758` `native_rq_poll` |
| 185 | primitive-into-reference | unattributed | `L` | `Int(0)` | `properties_sidetable.rs:1680` `props_defaults` |
| 59 | primitive-into-reference | unattributed | `L` | `Int(0)` | *(no native-builtins frame)* |
| 53 | primitive-into-reference | unattributed | `L` | `Int(0)` | `lang_invoke.rs:4536` `varhandle_compare_and_set` |
| 23 | primitive-into-reference | unattributed | `L` | `Int(0)` | `unsafe_natives_ext.rs:2426` `native_unsafe_cas_object` |
| 7 | primitive-into-reference | unattributed | `L`/`[` | `Int(0)` | `lib.rs:26586` `native_object_clone` |
| **2** | **pointer-into-primitive** | unattributed | **`I`** | **`Object(Some(..))`** | **`jca/provider_chain.rs:317` `make_provider`** — §5 |
| 12 | primitive-into-reference | unattributed | `L` | `Int(0)` | six singletons in `net_phase_e.rs` / `lib.rs` |

**Not one frame in `phases_early.rs` or `lang_system.rs`.** And note the shape
of the 465 + 185 + 53 + 23: every one is `Int(0)` **read** at an `L` slot —
`G30-1` §2.1's benign population, an untouched slot decoding as `Int(0)` and
`get_field_as(.., b'L')` correctly answering `null`. `native_rq_poll` is the
`ReferenceQueue.head` read `G30-1` measured; the other three are the same shape
at other empty slots.

---

## 3. MEASURED — a probe built to make them fire, which does not

A nine-vector corpus not touching a site is weak evidence; a probe written
specifically to drive every group-C/E class this lane writes is much stronger.
`G36Slots.java` does that — 37 assertions across `RoundingMode`, `MathContext`,
`ArrayList`, `HashMap`/`HashMap$Node`, `GregorianCalendar` and `URI` — run on
both VMs.

**37 of 37 rows are identical to HotSpot.** The load-bearing ones:

| case | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `RoundingMode.valueOf("FLOOR").name()` | `FLOOR` | `FLOOR` |
| `RoundingMode.values()` (all eight names) | `UP,DOWN,CEILING,FLOOR,HALF_UP,…` | identical |
| `RoundingMode.valueOf("UP") == RoundingMode.UP` | `true` | **`true`** |
| `new MathContext(7).getRoundingMode()` | `HALF_UP` | `HALF_UP` |
| `MathContext.DECIMAL64.getRoundingMode()` | `HALF_EVEN` | `HALF_EVEN` |
| `new BigDecimal("2.345").setScale(2, FLOOR)` | `2.34` | `2.34` |
| `new ArrayList<>(List.of(1,2,3)).size()` | `3` | `3` |
| `GregorianCalendar(2026,7,17).get(YEAR)` after `set` | `2027` | `2027` |
| `new URI("http://example.com/p").getPort()` | `-1` | `-1` |

The same run emitted **70** coercion events: 53 `native_rq_poll`, 16
`props_defaults`, 1 unattributed. **Zero from `phases_early.rs`.**

The third row is the one that settles the mechanism rather than just the
symptom. `register_phase52_rounding_mode`'s `valueOf` native **allocates a
fresh object** on every call. If it had run, `valueOf("UP") == RoundingMode.UP`
could not be `true` — `RoundingMode.UP` is a `getstatic` read of the constant
the real `<clinit>` built, and `E21-1` records that `getstatic` has no native
path. Identity holds, so the native did not run and the real enum answered.

---

## 4. SOURCE-VERIFIED — why: the registrars are synthetic-JDK-only

One call chain, one `#[cfg]`:

```text
register_builtins
  └─ #[cfg(feature = "synthetic-jdk")] register_synthetic_overrides   lib.rs:22259
       ├─ register_enterprise_final_natives   lib.rs:24696  → register_core_stdlib_extras (13 sites)
       ├─ register_phase51_natives            lib.rs:24702
       ├─ register_phase52_natives            lib.rs:24705  → math_context (5), rounding_mode (2),
       │                                                      time_enums (5), offset_datetime (5),
       │                                                      chrono_unit (2), byte_order (3)
       └─ register_phase53_natives            lib.rs:24708  → security (4), crypto (1)
```

`register_phase51_natives`, `register_phase52_natives` and
`register_phase53_natives` have **exactly one** caller each in the whole crate,
and it is the line above. `register_enterprise_final_natives` has two, the
second (`lib.rs:45229`) inside a test. `register_core_stdlib_extras` has three,
two of them in `phases_early.rs`'s own `#[cfg(test)] mod t2_tests`.

**The VM says the same thing, without being asked to interpret anything.**
`--dump-native-registry` under `--jdk-only` on the §3 probe run carries **10,691
rows**, and the number of rows for the classes these registrars own is:

| class | rows in a 10,691-row `--jdk-only` registry dump |
|---|---|
| `java/math/RoundingMode` | **0** |
| `java/math/MathContext` | **0** |
| `java/util/GregorianCalendar` | **0** |
| `java/net/Socket` | **0** |
| `java/time/DayOfWeek` | **0** |
| `java/time/LocalDateTime` | **0** |
| `java/time/temporal/ChronoUnit` | **0** |
| `java/util/EnumSet` | **0** |
| `java/util/HashMap$Node` | **0** |
| `javax/crypto/KeyGenerator` | **0** |

Three of the 77 sites' classes ARE registered under `--jdk-only`
(`java/util/ArrayList` 31 rows, `java/util/HashMap` 33, `java/net/URI` 29) —
but from `lib.rs`, `native-collections/src/lib.rs`, `net_phase_e.rs` and
`vm/`, **not** from `phases_early.rs`, and §3 drove all three hard with no
coercion.

This is `G34-1` §5.1's discovery about `register_http2_natives`, in a second
registrar family: **"not on this boot path" is true of `--jdk-only` and is not
the same as unreachable.** These bodies are live in a `synthetic-jdk` build,
where the classes are fabricated, their slot maps ARE the layout, and
`Value::Int(4)` at slot 0 of a fabricated `RoundingMode` is not a coercion at
all — it is the design. That is why deletion is wrong here too, and why a
"fix" that rewrites the slot indices to the real JDK layout would **break the
mode where the code actually runs** while changing nothing in the mode where
it does not.

### 4.1 What a fix would have had to be, and why it is not this lane's

For `RoundingMode` the correct value IS available (`"HALF_UP"` as a `String` at
slot 0, `4` as an `Int` at slot 1) and the reader is in the same file, so it
looks like the tractable case. It is not, for a reason that generalises to all
77: the object is allocated with `try_alloc_concurrent_synthetic(ctx,
"java/math/RoundingMode", 1)`, whose width is then `max`'d up to the real
class's field count under `--jdk-only` (`util_concurrent_ext.rs:1039`), so
writer and reader agree only because they share a **private slot map that is
not the real layout**. Correcting one end without the other is a silent wrong
answer, correcting both is a synthetic-JDK behaviour change, and no vector this
lane can run exercises the mode in which it would land. `G30-1` N5 calls this
"the shape the layout-aware writer was invented for", and that is the right
frame — it is a model migration, not a per-site value repair.

---

## 5. NEW — `pointer-into-primitive` fires, and `G30-1` predicted it would not

`G30-1` §4.1, on the worst of its four species:

> "a live `Object(Some(o))` at a primitive slot → **its own address, as a
> number**. The worst of the four: not merely wrong, non-deterministic, and it
> publishes a heap address into a Java `int`. **No site in the census produces
> it and none fired in the sweep**; it is counted because if it ever fires,
> nothing else in the VM will say so."

It fires. **`RCrypto`, twice per run, `native-builtins/src/jca/provider_chain.rs:317`,
in `make_provider`, species `pointer-into-primitive`, `descriptor=I`, value
`Object(Some(ObjectRef …))`.** The instrument `G30-1` added is what caught it,
on the first run of the first binary to carry it — which is the strongest
possible argument for that record's central choice to instrument before fixing.

The site is three consecutive writes, and its own comment names this lane's
file as the reason they exist:

```rust
// Synthetic fallback — populate the legacy slots 0/1/2 too so
// `phases_early::register_phase53_security` callers that haven't
// migrated to the real-JDK accessors still see consistent state.
ctx.set_field(p, 0, Value::Object(Some(n)));      // <- the pointer-into-primitive
ctx.set_field(p, 1, Value::Double(version));
ctx.set_field(p, 2, Value::Object(Some(info)));
```

The receiver `p` is a REAL `java.security.Provider` (the lines just above set
`initialized` by NAME, through the real-JDK accessor). So slot 0 is a real
primitive field and the Provider's name object is being written into it as an
address. The write is a compatibility shim for readers in `phases_early.rs` —
which §4 has just measured are not registered under `--jdk-only`. **The
consistency it maintains is with bodies that do not run in this mode**, and the
price is the worst species in the taxonomy.

`jca/provider_chain.rs` is not this lane's file. **N2.**

---

## 6. NOMINATIONS

**N1 — `scratchpad/g30/census2.py`: add a reachability column.** The census
ranks `phases_early.rs` first at 77 sites and every one is dead under
`--jdk-only`. The filter is cheap and already exists as data: cross the census's
`(class)` against a `--dump-native-registry` JSON, and separately mark any site
whose enclosing registrar is reachable only from
`#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`. Without it the
400 sites cannot be prioritised, and the next lane pointed at "the top file"
will spend its budget where this one did. Group A stays the right first target;
this is about groups E and F.

**N2 — `native-builtins/src/jca/provider_chain.rs:317`, the only measured
`pointer-into-primitive` in the tree.** §5. Two options, and the second is
better: (a) drop the three legacy slot writes, whose only stated purpose is
`phases_early::register_phase53_security`'s readers — which §4 measures are not
registered under `--jdk-only`; or (b) keep them under the same
`#[cfg(feature = "synthetic-jdk")]` gate as the readers they serve, so the mode
that needs them still gets them and `--jdk-only` stops publishing a heap
address into a Java `int`. Needs a `synthetic-jdk` build to verify either way,
which is why it is nominated. Note `RCrypto` is green today **with** the
defect — this is a latent-but-firing site, not an observed failure, and the
non-determinism is the reason not to leave it.

**N3 — `G30-1` N1 is still the highest value per line in this area, and this
lane can now say why from measurement.** Every one of the 2,321 events in §2 is
`class_id=-1 index=-1`; the only attribution available was a Rust backtrace,
which needs `CRATONVM_DBG_COERCION=1` and costs a `Backtrace::force_capture`
per event. Four one-line changes in `gc/src/gen_heap.rs` and
`gc/src/collector.rs` would put class and slot on the default-on warning and
make the backtrace unnecessary for triage.

**N4 — `java/util/ArrayList#1 elementData`, 62 sites tree-wide (21 here).**
`G30-1` N5 stands, unmodified and unclaimed by this lane: 21 of the 62 are in
`phases_early.rs` and all 21 are in synthetic-only registrars, so the live
majority is in the other files and that is where the layout-aware writer has to
land first.

**N5 — the `--jdk-only` reachability of `register_phase51/52/53_natives` is a
record of its own.** §4 establishes that three whole registrar phases —
`java.time` enums, `MathContext`/`RoundingMode`, `javax.crypto`/`java.security`
extras, `Calendar`/`TimeZone`/`Currency` — do not run under `--jdk-only` at all.
Nothing in this directory says so. It is load-bearing for anyone reading
`phases_early.rs` (25,929 lines, the crate's largest) and trying to work out
which half of it is live in the mode the branch is being judged in.

---

## 7. `git status --short` and what this lane did NOT do

```
$ git status --short native-builtins/src/phases_early.rs native-builtins/src/lang_system.rs
(no output — neither file is modified)
```

* **It changed no source.** §2–§4 are the reason; §4.1 is why the one
  apparently-tractable cluster is not tractable.
* **It did not touch `register_thread_local_natives`** or any other
  registration in `phases_early.rs`.
* It did not edit `INDEX.md`, `README.md`, the regression suite, or any file
  outside `docs/known-issues/jdk-only/`.
* It ran no `cargo` command and no state-changing git command.
* **It did not settle the 21 `ArrayList#1` sites' synthetic-mode correctness.**
  §3 shows they are inert under `--jdk-only`; whether the private slot map they
  use is right in a `synthetic-jdk` build is untested, because no binary
  available to this lane has that feature.
* It did not re-run `G30-1` §2's `CRATONVM_DBG_OVERLAY` sweep. §2's
  `CRATONVM_DBG_COERCION` sweep is a different instrument answering a different
  question, and the two tables should not be compared row for row.

## 8. Vectors, MEASURED on `target-rel3` (`9ae371468`)

All seven the assignment named, plus two more, run under `--jdk-only`. The
binary is unchanged across this lane, so these are a baseline, not an after —
but they are the first measurement of them on this commit.

| vector | result |
|---|---|
| `RJdkHello` | `PASS`, `checks=41` |
| `RJdkLogging` | `PASS`, `checks=79` |
| `RJdkExecutors` | `PASS`, `checks=69` |
| `RStrings` | `PASS`, `checks=46` |
| `RJdkNet` | `PASS`, `checks=81` |
| `RCrypto` | `PASS`, `checks=57` — was `exit 1` in `G30-1` §2 |
| `RSimpleTimeZoneRaw` | `PASS`, `checks=393` |
| `RJdkCollections` | `PASS`, `checks=69` |
| `RJdkIntrinsics3` | `PASS`, `checks=1011` |
