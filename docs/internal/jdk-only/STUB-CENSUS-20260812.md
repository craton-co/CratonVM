# 2026-08-12 — the stub census: what `--jdk-only` still refuses, and what it does not

**What this is.** A per-registration census of CratonVM's remaining
`NativeKind::SyntheticStub` surface, plus the population that is *not*
`SyntheticStub` and therefore **runs in strict mode**, measured from a booted VM
rather than from source. It feeds a roadmap whose goal is *remove the stubs so
that any Java application runs under `--jdk-only`*.

**Read §1 before any number below.** Every count here is a property of a
particular instrument run, and the instrument turns out to already exist and to
already work. The headline finding is not a count at all — it is that the
question "which natives shadow real JDK bytecode in strict mode" has a
one-command answer, and the answer is **4,455**, not the ~1,200 the stub ratchet
tracks.

**This document is a census, not an adjudication.** It says what is registered
and, where it could run a program, what happens. It does not say a given row is
a defect. Several families measured here are correct exactly as they are.

**Three findings that change how the roadmap should be planned:**

1. **The instrument exists and works** (§1). `--dump-native-registry <FILE>` plus
   `--explain-jdk-only` yields the per-row census this campaign lacked, from a
   shipping binary, with no rebuild.
2. **Retiring a stub does not require touching its registrar** (§2.3). A central
   re-tag table converts a `Bridge` to a `SyntheticStub`, which `--jdk-only` then
   refuses. That makes most of the work collision-free and therefore
   parallelisable.
3. **Category (B) was not merely empty in the tested families — it ran
   backwards** (§5.2). `Collections.synchronizedList` *loses elements* in
   compatible mode and is exact in strict mode. Deleting stubs is not only safe
   for these families; it is a correctness fix.

---

## 1. The instrument — YES, and here is the command

> The brief recorded that `--dump-native-registry` "produced NO output when the
> orchestrator tried it". **It works.** It takes a mandatory `<FILE>` operand,
> writes at VM shutdown, prints nothing to stdout, and prints one confirmation
> line to stderr. Two ways to get "no output", both reproduced on
> `cratonvm-f8.exe`:
>
> 1. **The flag placed after the main class is silently ignored.** VM options
>    must precede `<MainClass>`; anything after it is a *program* argument.
>    `cratonvm -cp census Hello --dump-native-registry after.json` prints
>    `HELLO-OK`, exits 0, writes **no file**, and emits **no warning**. This is
>    the failure mode that looks exactly like a broken flag.
> 2. **The confirmation line is buried.** stderr carries hundreds of
>    `WARN cratonvm::gc::guard` lines on a default run; the single
>    `[cratonvm] wrote native registry census …` line scrolls past unless you
>    grep for it.
>
> Omitting the operand is *not* one of the ways — `clap` rejects it cleanly with
> `error: a value is required for '--dump-native-registry <FILE>'`. It does not
> swallow the following `-cp`.

A per-row `(class, method, descriptor, kind, registrar)` census **can be
extracted from a shipping binary today**, with no source change and no rebuild:

```
cratonvm --jdk-only --explain-jdk-only \
         --dump-native-registry census-strict.json \
         -cp <cp> <MainClass>
```

Confirmation line on stderr, from the run this document is built on:

```
[cratonvm] wrote native registry census (schema 3, image-adjudicated) to
census/reg-strict-adj.json (intrinsic=645, bridge=9781, synthetic-stub=0)
```

> **Read `intrinsic=645` as ROWS (C18, 2026-08-12).** Every number in this
> census is a registry-row count, which is what the dump emits. The
> **distinct-triple** count behind those 645 rows is **614** — 31 rows are
> duplicate registrations of a triple already registered. Coverage arithmetic
> must use 614; deletion arithmetic must use 645. `W8-C3-1` §"The coverage
> arithmetic" has the recomputation; `W7-95` originally read 645 as triples and
> now carries a correction banner.

Each row carries more than the five fields asked for:

| field | meaning |
|---|---|
| `class` / `name` / `descriptor` | the triple |
| `kind` | `intrinsic` \| `bridge` \| `synthetic-stub` — the **effective** kind, after the central re-tag of §2.3 |
| `registered_by` | registrar `file:line` — the deletion target |
| `overwrote` | the row this registration displaced (last-write-wins chronology) |
| `owns_slot` | whether this row is the one that dispatches; **losers are retained in the file** |
| `invocations` | dispatch count for THIS run — liveness, not reachability |
| `kind_stated` / `kind_chosen` | whether the kind was explicit or ambient; whether it won the merge |
| `real_declaring_method` | adjudication against the *loaded* class (only populated if the class loaded this run) |
| `image_declaring_method` | adjudication against the **bytes on the class path**, independent of what loaded — `declared`, `acc_native`, `has_code`, `inherited_from`, `inherited_acc_native`, `inherited_has_code`, `inherited_abstract` |

`image_declaring_method` is `null` unless `--explain-jdk-only` is also passed.
**Pass it.** Without it the census cannot answer the only question that matters
for this roadmap — *does the JDK ship working bytecode for this triple?* — and
every four-way split below collapses.

### 1.1 Two corrections to the folklore around this flag

* **The doc comment is ahead of the binaries.** `vm-cli/src/main.rs:514` documents
  `schema_version` 4 and both shipping binaries emit rows in schema-4 shape
  (`kind_stated`, `kind_chosen`, `owns_slot`, `image_declaring_method` are all
  present, and the JSON `"schema_version"` field reads `4`) — but the stderr
  confirmation line still says **"schema 3"**. The banner is stale, the payload
  is not. Do not use the banner to decide whether a dump is usable; read
  `natives[0]` instead.
* **The census is not the stub ratchet, and the two numbers must not be
  compared.** The ratchet builds a registry *in-process from the boot path only*
  (see §6); the census reflects a *whole booted VM*, which registers more. The
  ratchet's 1261 and this document's 1282 are both correct and measure different
  populations. Quoting one against the other is the mistake that produced the
  "+6 unattributed".

### 1.2 The rest of the toolchain already exists

`--dump-native-registry` is not a lone flag; it has consumers already in-tree
that this census did not need to reinvent:

* `scripts/jdk-only-census.sh` — one VM run per policy, four dumps per run, and a
  refusal to let a permissive registry be paired with a strict class census.
* `scripts/jdk-only-kind-map.py` — freezes each row's *kind* so an ambient
  `set_category` edit cannot re-tag a thousand natives in silence.
* `scripts/jdk-only-bridge-ratchet.py` — freezes `bridge.without_acc_native` and
  `bridge.shadows_bytecode`, which are exactly the §3(C) counts below.
* `scripts/jdk-only-no-image-receivers.py`, `jdk-only-dead-sweep.py`,
  `jdk-only-interception.py`, `jdk-only-adjudicate.py`.

**Nothing is missing from the instrument.** The gap this campaign actually has is
that the census was not being *run* against the strict binary and read per-row.

### 1.3 The one thing the instrument still cannot do

`invocations` is per-run, so "is this row dead?" is only answerable relative to a
workload. A row with `invocations: 0` under this document's probes is **not**
proven dead — W7-88-net-channels-dead-registration.md
records a row that survives four configurations without ever dispatching. The
smallest change that would close this: a build that accumulates `invocations`
across a corpus run into one merged census. `regression-suite/bridge-ratchet.sh`
is the natural host. **Not attempted here** — it needs a corpus run this lane
does not own.

---

## 2. Definitions, and the mechanism that reframes the roadmap

### 2.1 What `--jdk-only` actually drops

Measured, both binaries, same trivial program:

| binary | mode | intrinsic | bridge | synthetic-stub |
|---|---|---|---|---|
| `cratonvm-f8` (current) | compatible | 645 | 9781 | **1282** |
| `cratonvm-f8` | `--jdk-only` | 645 | 9781 | **0** |
| `cratonvm-control-44044c7e2` (pristine dev) | compatible | 645 | 9741 | **1280** |
| `cratonvm-control-44044c7e2` | `--jdk-only` | 645 | 9741 | **0** |

(The `intrinsic` column is **rows**, not distinct triples — 645 rows = 614
distinct triples. See the note in §2.)

`--jdk-only` drops **every** `SyntheticStub` and **nothing else**. So the
premise of the brief's category (C) is exactly right: a stub that runs in strict
mode is a contradiction in terms, and the only way one runs is by not being
tagged `SyntheticStub` in the first place.

### 2.1a The three kinds, and why "untagged" means "stub"

`NativeKind` has exactly three variants (`native-api/src/registry.rs:4596-4600`):
`Intrinsic`, `Bridge`, `SyntheticStub`. `allowed_in(mode)` (`:4624`) makes
`SyntheticStub` the only one `JdkOnly` rejects.

**There is no default kind.** `current_category` is `Option<NativeKind>` and is
`None` outside any scope; `effective_category()` (`:5509`) falls back to
`SyntheticStub`. That fallback is the conservative choice — an untagged
registration stays visible to the audit and gateable rather than being silently
trusted — and it is why "a stub" and "a registration nobody adjudicated" are the
same population. The census's `kind_chosen` column reports which rows took that
fallback.

The prevailing idiom is manual save/restore (`set_category(__prev_cat)`), not the
scoped `with_category` (`:5516`), which is used in roughly a dozen places. That
is what makes the kind ambient across hundreds of lines and why
`scripts/jdk-only-kind-map.py` exists.

**One more bucket that is neither (A)–(D):** in real-JDK mode the VM calls
`set_drop_real_layout_synthetic(true)` (`vm/src/vm/vm_init.rs:1918`, API at
`native-api/src/registry.rs:5462`), so some registrations *compiled into the
shipping binary* are dropped at registration time before they ever reach the
census. Those rows are invisible to `--dump-native-registry` too.

### 2.2 Registrations vs. slots

1282 stub *registrations* resolve to **1154 stub-owned dispatch slots**; the
other 128 lost their slot to a later registration in compatible mode too. Both
numbers are in this document and they are not interchangeable. Deletion work is
sized by *registrations* (each is a line of Rust); behavioural risk is sized by
*slots* (only a slot owner can be called).

### 2.3 The re-tag — the roadmap's actual lever

`native-api/src/registry.rs:5782-5798` applies a central, image-adjudicated kind
override *before* `register_inner` sees the row:

```rust
if self.effective_category() == NativeKind::Bridge
    && (crate::no_image_receiver::receiver_declared_by_no_supported_image(class_name)
        || crate::retired_shadow::triple_is_retired_shadow(class_name, method_name, descriptor))
{
    // ... force NativeKind::SyntheticStub, then restore
}
```

This matters more than any count here. **Retiring a shadow does not require
touching its registrar.** Adding a triple to `RETIRED_SHADOW_TRIPLES`
(`native-api/src/retired_shadow.rs:215`, 88 entries, sorted and binary-searched)
re-tags it `SyntheticStub`, which makes `--jdk-only` refuse it and the real JDK
bytecode run — from one table, centrally, with no registrar edit and therefore
no collision with any other lane. Deleting the now-dead Rust becomes a separate,
purely mechanical follow-up.

The census reports the split directly: of 1282 stub registrations, **396 are
`kind_stated: true`** (explicitly tagged or centrally re-tagged) and **886 are
ambient** (born a stub at their site). 113 of the 396 are `java/util/logging` —
the retirement already in flight.

The companion module `native-api/src/no_image_receiver.rs` does the same at
*class* granularity — a whole receiver the image never declares — through three
tables: `NO_IMAGE_JDK_RECEIVERS` (49 entries, line 130),
`VM_MINTED_STAND_IN_RECEIVERS` (8, line 196) and `VM_SERVICE_RECEIVERS` (9, line
233). A fourth, `STRICT_STILL_FABRICATES` (line 317), is **empty** — which is the
claim that strict mode fabricates no receiver, and is worth re-checking against
§4.3's 335 fabricated *methods* on non-fabricated classes, a shape that table
does not cover.

### 2.4 The four image shapes, and why "stub" is the wrong axis

The interesting axis is not the kind, it is what the JDK image says about the
triple. Across all **9,342 slot-owning rows in `--jdk-only`**:

| image shape | bridge | intrinsic | total | what it means |
|---|---:|---:|---:|---|
| **SHADOWS real bytecode** | 3956 | 499 | **4455** | JDK ships working code; we run ours instead |
| inherited-only | 1631 | 20 | 1651 | class doesn't declare it; we intercept the inherited method |
| declared abstract / no code | 1209 | 5 | 1214 | registration on an interface or abstract method |
| no such class in image | 914 | 81 | 995 | CratonVM-internal + third-party shims |
| **real `ACC_NATIVE`** | 690 | 2 | **692** | legitimate — the JDK *requires* the VM to supply these |
| class in image, method NOT declared | 335 | 0 | 335 | a method the JDK does not have — fabricated |

**Only 692 of 9,342 strict-mode natives are the kind a JVM is obliged to
provide.** That framing, not the 1,282, is the roadmap's real scope.

---

## 3. The four-way split

Counting method, stated so it can be checked: one `cratonvm-f8.exe` run per mode
over a trivial `Hello` class plus a 54-probe program, with
`--explain-jdk-only --dump-native-registry`; rows classified by `owns_slot`,
`kind`, and `image_declaring_method`. Family grouping is by class-name prefix and
is **approximate at the boundaries** (a `sun.security.*` row could sit in "JCA"
or "NIO"); the totals are exact, the family attributions are not.

| | population | count | basis |
|---|---|---:|---|
| **(A)** | refused in strict, real bytecode works | **≥ 335 slots, families below all-green on probes** | tested, §5 |
| **(B)** | refused in strict, real bytecode then FAILS | **0 found in the tested families** — and one family runs *better* in strict (§5.2) | tested, §5 — and see the honesty note |
| **(C)** | runs in strict, shadows real bytecode | **4455 slots** (3956 bridge + 499 intrinsic) | measured, §4 |
| **(D)** | `--features synthetic-jdk` only | **0 census rows by construction**; ≈4390 source call sites in 282 synthetic-exclusive registrars | source walk, §3.1 |

Plus two populations the brief's four-way split has no box for, which the census
forces into view:

| | population | count |
|---|---|---:|
| **(E)** | runs in strict, method the JDK image does **not declare** | **335 slots** — fabricated surface, dead or wrong |
| **(F)** | runs in strict, intercepts an **inherited** method or an **abstract/interface** declaration | **2865 slots** |

### 3.1 (D) is invisible to this instrument, but it can be sized from source

A registrar reachable only from `register_synthetic_overrides` is not called in
either shipping binary, so it contributes **zero** census rows — which is exactly
the point: **deleting category (D) changes nothing a user sees.**

The gate is a cfg *and* a runtime check, both required:

* `native-builtins/src/lib.rs:21525` — `#[cfg(feature = "synthetic-jdk")]` on
  `register_synthetic_overrides` (`:21526`, body to `:24307`).
* Its only non-test caller is `register_builtins` (`native-builtins/src/lib.rs:21519`),
  itself gated at `:21514`.
* Whose only production caller is `vm/src/vm/vm_init.rs:1839`, inside
  `#[cfg(feature = "synthetic-jdk")]` (`:1835`) **and** `if config.use_synthetic_jdk`
  (`:1837`).
* `synthetic-jdk` is in no crate's default feature set. The default build compiles
  the `#[cfg(not(feature = "synthetic-jdk"))]` arm at `vm/src/vm/vm_init.rs:2472`,
  which calls only `register_essential_natives_with_shims` (`:2498`).
* In real-JDK mode `vm/src/native/builtins.rs:28-29` supplies an **empty no-op
  shim** of the same name, so the symbol resolves and the body is gone.

**Sizing, from a source walk — approximate, and the method matters:**

| measure | count |
|---|---:|
| registrars transitively reachable from `register_synthetic_overrides` | 710 |
| of those, **also** reachable from the shipping path | 428 |
| **synthetic-exclusive registrars = category (D)** | **282** |
| `register(` calls inside those 282 | ~4117 |
| inline `registry.register(...)` calls in the function body itself | 273 |
| **(D) total source call sites** | **≈4390** |
| `register(` calls sitting *directly* under a `#[cfg(feature = "synthetic-jdk")]` attribute | ~621 |

**The 428-registrar overlap is the load-bearing correction.** Thirty registrars
are called from *both* `register_synthetic_overrides` and
`register_essential_natives_with_shims` — `register_unsafe_natives`
(`lib.rs:14609` shipping / `:23907` synthetic), `register_lock_support_natives`
(`:17252` / `:23910`), `register_net_natives` (`:7930` / `:23944`),
`register_math_natives`, `register_base64_natives`, `register_uuid_natives` and
24 others. **None of these are category (D)**, and deleting one because it
appears in the synthetic list would break the shipping binary. Any (D) deletion
must check both call sites first.

Source call sites are not net registry entries — synthetic registrations
deliberately overwrite triples the essential path already registered. Command
that would give the true runtime delta, **not run in this lane**:

```
cargo build -p cratonvm-cli --features synthetic-jdk
<that binary> --explain-jdk-only --dump-native-registry syn.json -cp census Hello
# net (D) = rows in syn.json - rows in reg-compat-adj.json (11708)
```

`vm/src/vm/vm_init.rs:3664-3666` states the delta as "~3,100 in real-JDK mode,
~5,200 with `synthetic-jdk`". **That comment is stale** — this census measures
**11,708** registrations in real-JDK/compatible mode, not ~3,100. Do not use it
to size anything; run the command.

### 3.2 (A)/(B) by family — the refused surface

The 1154 stub-owned slots in compatible mode, grouped. `image` column: how many
of the family's slots name a class the JDK image actually has.

| family | slots | in image | what strict mode falls back to |
|---|---:|---:|---|
| CratonVM internal (`cratonvm/*`, `CratonVM$*`) | 370 | 0 | nothing — no user code names these |
| `java.util` collections | 139 | ~134 | real `java.util` bytecode |
| netty / `tcnative` / `tomcat.jni` shims | 118 | 0 | the real library's own bytecode |
| **`java.util.logging`** | 90 | 90 | real JUL bytecode — **retirement in flight**, §6 |
| slf4j / jboss-logging shims | 52 | 0 | the real library |
| `java.net` + URL codecs | 42 | ~42 | real bytecode |
| NIO / filesystem | 41 | ~39 | real bytecode |
| `java.lang` / process | 40 | ~40 | real bytecode |
| `j.u.c.atomic` | 39 | ~39 | real bytecode |
| Spring bootstrap shims | 37 | 0 | Spring's own bytecode |
| **`j.u.c.locks` (StampedLock)** | 31 | 30 | real `StampedLock` |
| `jdk.internal` / `sun.misc` | 26 | ~26 | real bytecode |
| JCA / crypto | 24 | ~24 | real JCA |
| Quarkus bootstrap shims | 21 | 0 | Quarkus' own bytecode |
| `j.u.c` core (latch, barrier, executor) | 21 | ~21 | real bytecode |
| `java.time` (`Instant`) | 16 | 16 | real `java.time` |
| `java.io` | 13 | ~13 | real bytecode |
| JMX / management | 12 | ~12 | real bytecode |
| `java.util.function` | 11 | ~11 | real bytecode |
| misc JDK (`zip`, `httpserver`, `rmi`, `stream`, `invoke`) | 10 | ~10 | real bytecode |
| other third-party | 1 | 0 | the real library |

**818 of the 1154 name a class the JDK image does not have at all.** Those can
never be a `--jdk-only` correctness question; they are Compatible-mode
scaffolding for third-party libraries plus CratonVM's own internal classes. The
`--jdk-only` roadmap's real refused surface is the **335 slots on 50 real JDK
classes**, enumerated in §5.

---

## 4. The (C) population — stubs that RUN in strict mode

**4455 slot-owning natives dispatch in `--jdk-only` on triples for which the JDK
image ships `Code`.** By registrar file, which is the unit of parallel work:

| registrar | (C) rows | dominant families |
|---|---:|---|
| `native-builtins/src/lib.rs` | 657 | `java.lang.Object`/`Class`/`System`, `java.util.Objects`, `Arrays` |
| `native-collections/src/lib.rs` | 563 | `ArrayList`, `HashMap`, `ConcurrentHashMap`, iterators |
| `native-builtins/src/lang_math.rs` | 287 | `Math`, `Character`, boxed `valueOf` |
| `native-builtins/src/lang_string.rs` | 171 | `String`, `StringBuilder` |
| `native-builtins/src/phases_late/nio_file.rs` | 161 | `java.nio.file` |
| `native-builtins/src/phases_late/foreign_ffm.rs` | 159 | `java.lang.foreign`, `jdk.internal.foreign` |
| `native-builtins/src/util_concurrent_ext.rs` | 151 | `j.u.c` |
| `native-builtins/src/net_phase_e.rs` | 143 | `java.net` |
| `native-io/src/lib.rs` | 122 | `java.io` |
| `native-awt/src/natives.rs` | 103 | `java.awt`, `sun.java2d` |
| `native-builtins/src/phases_early.rs` | 98 | `StringLatin1`, `ArraysSupport` |
| `native-builtins/src/reflect_annotations.rs` | 84 | `java.lang.reflect` |
| `native-builtins/src/unsafe_natives_ext.rs` | 84 | `jdk.internal.misc.Unsafe` |
| `native-builtins/src/lang_invoke.rs` | 82 | `java.lang.invoke` |
| `native-builtins/src/shared_secrets_bridge.rs` | 75 | `jdk.internal.access` |
| `native-builtins/src/unsafe_natives.rs` | 66 | `sun.misc.Unsafe` |
| `native-builtins/src/jmx.rs` | 65 | `javax.management` |
| `native-builtins/src/http_url_connection.rs` | 58 | `sun.net.www.protocol` |
| `native-builtins/src/servlet.rs` | 58 | `java.nio.ByteBuffer` |
| `native-io/src/socket_channel.rs` | 55 | `sun.nio.ch` |
| `native-builtins/src/lang_reflect.rs` | 49 | `java.lang.reflect` |
| `native-builtins/src/phases_late/concurrent.rs` | 48 | `j.u.c` |
| `native-builtins/src/phases_late/ssl_security.rs` | 45 | `javax.net.ssl` |
| `native-builtins/src/jca/cipher.rs` | 40 | `javax.crypto` |
| `native-builtins/src/t27_tls.rs` | 40 | `sun.security.ssl` |
| *(remaining ~40 files)* | ~1000 | long tail |

### 4.1 (C) is two very different things, and the census separates them

**499 are `kind: intrinsic`** and are overwhelmingly *semantics-preserving
accelerations* — 308 in `java.lang` and 34 in `java.math`, dominated by
`lang_math.rs`. `Math.log`, `Integer.numberOfTrailingZeros`,
`ArraysSupport.vectorizedHashCode`, `StringLatin1.toLowerCase`. A JVM shadowing
`Math.log` with a native is not a divergence; it is what a JVM *is*. These are
**not roadmap work** unless a specific one is measured wrong.

**3956 are `kind: bridge`** and each is an unadjudicated claim that our Rust
matches the JDK's Java. That is the population that makes `--jdk-only` diverge
from HotSpot. It is also where `scripts/jdk-only-bridge-ratchet.py`'s
`bridge.shadows_bytecode` counter already points — the gate exists; what is
missing is the drawdown.

### 4.2 (C) rows that actually FIRED

Reachability is not liveness. Under the 54-probe program in strict mode, **398
slots dispatched, 7186 calls**, of which **325 shadow real JDK bytecode**. The
top of that list is the honest picture of what strict mode is really doing:

| calls | kind | triple | registrar |
|---:|---|---|---|
| 1399 | bridge | `java.lang.Object.<init>()V` | `native-builtins/src/lib.rs:10247` |
| 759 | intrinsic | `java.util.Objects.requireNonNull(Object)` | `native-builtins/src/lib.rs:29430` |
| 505 | bridge | `java.lang.Enum.<init>(String,I)V` | `native-builtins/src/lib.rs:16322` |
| 342 | bridge | `java.lang.Enum.ordinal()I` | `native-builtins/src/lib.rs:16346` |
| 166 | bridge | `java.util.HashMap.put` | `native-collections/src/lib.rs:8500` |
| 161 | intrinsic | `java.lang.Math.floorMod(II)I` | `native-builtins/src/lang_math.rs:194` |
| 138 | intrinsic | `jdk.internal.util.ArraysSupport.vectorizedHashCode` | `native-builtins/src/phases_early.rs:22734` |
| 135 | bridge | `java.util.ArrayList.add` | `native-collections/src/lib.rs:4216` |
| 130 | bridge | `java.util.ArrayList.size` | `native-collections/src/lib.rs:4207` |
| 93 | bridge | `jdk.internal.util.Preconditions.checkFromToIndex` | `native-builtins/src/preconditions.rs:410` |
| 89 | bridge | `java.util.Properties.getProperty` | `native-builtins/src/properties_sidetable.rs:3631` |
| 80 | bridge | `java.lang.Class.desiredAssertionStatus` | `native-builtins/src/lib.rs:11185` |
| 54 | bridge | `java.io.PrintStream.println(String)` | `native-builtins/src/logging_shims.rs:131` |
| 54 | bridge | `java.lang.ref.ReferenceQueue.poll` | `native-builtins/src/reference.rs:207` |

`Object.<init>`, `Enum.<init>`/`ordinal`, `ArrayList`, `HashMap` and
`PrintStream.println` are the load-bearing core of (C). **`Object.<init>` and
`Enum` are almost certainly not removable** — they are entangled with object
layout and the GC header, not with class-library convenience. `ArrayList`,
`HashMap`, `Properties` and `PrintStream` are candidates, and they are the ones
where the divergence risk is highest because they are hit thousands of times per
program.

### 4.3 The 335 fabricated methods (population E)

335 strict-mode slots register a `(class, method, descriptor)` where the class
exists in the image and **does not declare that method**. Concentrated in
`sun.nio.ch` (53), `java.lang` (50), `jdk.internal.misc` (30), `java.util` (27),
`java.nio.channels` (21), `sun.nio.fs` (18), `java.lang.foreign` (17). These
cannot be called by real JDK bytecode via normal dispatch, so they are either
dead weight or reachable only through a CratonVM-internal path — the exact shape
W7-88-net-channels-dead-registration.md documents and
`scripts/jdk-only-dead-sweep.py` exists to sweep. **Cheapest lane in the
roadmap**, and it collides with nobody.

---

## 5. Does the real JDK bytecode serve? — tested, three ways

Method: one 54-probe Java program over the families of §3.2 that name real JDK
classes, run on **HotSpot JDK 25.0.3.9** (the oracle), on `cratonvm-f8.exe`
(compatible), and on `cratonvm-f8.exe --jdk-only`, comparing printed results
string-for-string. Plus a second, 9-check multi-threaded correctness program —
counting operations, never wall-clock.

### 5.1 Functional probe: 52/54 identical in BOTH modes

| family | probes | strict == HotSpot? |
|---|---:|---|
| `java.util.logging` (getLogger, levels, LogRecord, Handler, LogManager, parent chain) | 8 | yes |
| `StampedLock` (write excl, optimistic read + invalidation, views, convert) | 5 | yes |
| `Collections.synchronized{Map,Set,List,Collection}` + `unmodifiableMap` | 6 | yes |
| `Comparator` / `List` / `Map` / `Set` statics and defaults | 8 | yes |
| `StringJoiner` (prefix/suffix, emptyValue, merge) | 3 | yes |
| `CountDownLatch` / `CyclicBarrier` / `AtomicBoolean` | 4 | yes |
| `java.time.Instant` | 5 | yes |
| `URLEncoder` / `URLDecoder` | 3 | yes (see note) |
| `ServiceLoader`, `ProcessHandle`, `StreamSupport`, `CodingErrorAction`, `ZipEntry`, `Security` | 7 | yes |
| JCA — AES/ECB ciphertext, AES/CBC round trip, SHA-256, providers | 4 | yes |
| `MethodHandleProxies.asInterfaceInstance` | 1 | **no — fails in both modes** |

The two non-matches:

* `urlenc.rt` — a **console encoding artifact**, not a divergence. All three
  runtimes print the same bytes; the non-ASCII round-trip is mangled identically
  by the pipe on this Windows host. Not a finding.
* `mhproxies` — **fails in both modes, differently**: compatible throws
  `ClassCastException: MethodHandle cannot be cast to Runnable`; strict throws
  `ClassFormatError: ldc: unsupported constant pool entry type at #26`. Strict's
  failure is a **class-file parsing gap, not a stub gap** — it is the real
  `MethodHandleProxies` bytecode hitting an `ldc` constant-pool form the VM does
  not parse. This is a genuine `--jdk-only` blocker and it is *not* in any stub
  population. See §8.

### 5.2 Concurrency probe: strict 9/9 — and strict is MORE correct than compatible

The families most at risk of being load-bearing — where a stub might be doing
real synchronisation that real bytecode cannot do on this VM — all hold in
strict mode. **Compatible mode does not.**

| check | HotSpot | `--jdk-only` | compatible |
|---|---|---|---|
| `synchronizedMap` 4×2000 puts, size | 8000 | **8000** | 8000 |
| `synchronizedList` 4×2000 adds, size | 8000 | **8000** | **6534 / 4697 — LOSES DATA** |
| `StampedLock.writeLock` mutual exclusion, 4×2000 increments | 8000 | **8000** | 8000 |
| `StampedLock` read/write torn-pair observations, 20000×3 readers | 0 | **0** | 0 |
| `ReentrantReadWriteLock` exclusion (control) | 8000 | **8000** | 8000 |
| `CountDownLatch` rendezvous (before / after / await) | 0 / 4 / true | **0 / 4 / true** | 0 / 4 / true |
| `CyclicBarrier` 4-party × 50 passes | 200 | **200** | **run stops here, both runs** |
| `AtomicBoolean` CAS — exactly one winner × 500 rounds | 500 | **500** | not reached |
| `ConcurrentHashMap.merge` under contention | 8000 | **8000** | not reached |

Compatible-mode figures are from two runs; the two `synchronizedList` values are
the two runs' results, not a range. Read the provenance note below the bullets
before quoting the `CyclicBarrier` row.

**This inverts the brief's category-(B) worry for these families.** The concern
was that refusing a stub might make strict mode *silently worse*. Measured, the
opposite holds:

* **`Collections.synchronizedList` loses elements in compatible mode.** Two runs,
  6534 and 4697 of 8000 — a different number each time, so a genuine race in the
  stub, not a fixed off-by-N. Strict mode, running the real JDK's
  `SynchronizedRandomAccessList`, returns 8000 exactly. This is the
  `Collections.synchronized*` identity-stub-is-not-atomic defect, measured.
* **The compatible run then stops** at the 4-party `CyclicBarrier`; the last
  three checks never print. Strict mode completes all nine.

  **Provenance, because this one claim is spliced from two runs and one of them
  was killed.** Run 1 ended on its own — the shell pipeline closed and returned,
  so the VM process exited — after exactly six probe lines. Run 2 stopped at the
  same six lines and its stderr carries one
  `Thread Thread-5 terminated with error: ExceptionThrown(...)
  (dispatchUncaughtException also failed: ...)`, but run 2 was **killed by the
  harness**, so its truncation is not by itself evidence. What is established:
  **two runs stopped at the same probe**, and a thread died with a
  double-fault on the uncaught-exception path in the one run whose stderr was
  captured. What is **not** established: the VM's exit code (the pipeline
  returned `grep`'s status, not the VM's), and whether the thread death is the
  cause of the stop or a separate symptom. Re-run with stderr kept and
  `echo ${PIPESTATUS[0]}` before treating this as a filed defect.

**`StampedLock` is the other important row.** 31 stub slots, refused in strict,
and the real JDK `StampedLock` bytecode provides correct mutual exclusion *and*
correct optimistic-read invalidation on this VM.
W6-12-stampedlock-split-brain.md is a *compatible-mode* defect; strict mode
does not have it.

Counting operations, never wall-clock: no timing assertion appears in this
probe, so it is not sensitive to host load.

### 5.3 What this licenses, and what it does not

**Licensed:** for the eleven families in §5.1 and the nine checks in §5.2, the
real JDK bytecode serves, and category **(B) is empty**. Those stub registrars
can be retired via `RETIRED_SHADOW_TRIPLES` and then deleted.

**Not licensed:** these are happy-path, single-process, short-lived probes. They
do not exercise `FileHandler` writing to disk, `LogManager.readConfiguration`,
`ProcessBuilder.start` on a real child, TLS handshakes, or NIO async close. A
green probe closes a headline, not a family. And the 818 no-class-in-image stubs
(netty, slf4j, Quarkus, Spring, tcnative) were **not tested at all** — they need
their libraries' jars, which this lane does not own.

---

## 6. The stub-ratchet drift: all +8 attributed, none unexplained

The brief records `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT = 1261` against a
frozen 1253, "+8, of which only +2 is attributed". **The +6 is attributed, in the
ratchet test's own body.** `native-builtins/tests/stub_ratchet.rs:466-474` names
it as cause (a): six `java/util/logging/` registrations that became
`SyntheticStub` because they were added to `RETIRED_SHADOW_TRIPLES` — **a
retirement, not a new fake.** Both edits are live:
`native-api/src/retired_shadow.rs:239-240` (`LogManager.getLogManager`,
`LogManager.getLogger`) and `:258-261` (the four `LogRecord` source-pair triples,
retired as a set on 2026-08-12).

The arithmetic closes exactly: `stub_ratchet.rs:450` **predicts** 1259
no-management. Observed 1261 = predicted 1259 + 2 Cipher. `1253 + 6 + 2 = 1261`.
The "+6 unattributed" was measuring drift from the **stale 1253** rather than
from the **predicted 1259**.

Corroborated independently by this census: five `java/util/logging` triples
(`Handler.getLevel`/`setLevel`, `LogRecord.getLevel`/`getMessage`/
`getSequenceNumber`) are the **only** triples in the entire registry where a
`synthetic-stub` owns the slot in compatible mode and a surviving `intrinsic`
owns it in strict mode — the fingerprint of a retirement mid-flight.

### 6.1 Two things to keep off the ledger

* `stub_ratchet.rs:475-482` records that the four new scalar `StringBuilder.insert`
  overloads move this number by **zero** — ambient `Bridge` at the site.
* The 7-row `java/io/Print*` retirement is **not landed**;
  `retired_shadow.rs:135-168` describes it as held, and no `java/io/PrintWriter`
  entries exist in the table.

### 6.2 The ratchet number cannot be recomputed without cargo

`BASELINE_SYNTHETIC_STUBS*` is a **runtime** count — `stub_ratchet.rs:586-605`
builds a live `NativeMethodRegistry`, calls the boot-path registrars, and counts
rows from `dump_registrations()`. `NO_MANAGEMENT` is a `#[cfg(feature =
"management")]` gate (`native-builtins/tests/common/vm_init_boot_path.rs:293-305`),
not a text exclusion. It is not reproducible by source scanning, for reasons the
test file itself enumerates and forbids at `stub_ratchet.rs:81-86`: registrars
loop over const tables so one `.register(` token yields 1 or 200 rows; the kind
is ambient and set by an ancestor frame; the §2.3 re-tag overrides the site's
kind from two tables; drop arms subtract rows on env vars; `alias_class`
(`registry.rs:7495`) synthesises rows by replaying the log; and duplicate rows
count, so the answer depends on call order.

**Do not hand-derive it.** The only two ways to get it:

```
cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
cargo test -p cratonvm-native-builtins --test stub_ratchet --features management -- --nocapture
```

---

## 7. Parallelisable work breakdown

Lanes below are disjoint **by registrar file**, which is the collision unit. A
lane that only adds rows to `RETIRED_SHADOW_TRIPLES` collides with nothing — but
two such lanes collide with *each other* in that one table, so the table is
serialised into a single lane (L0) that batches other lanes' nominations.

| lane | scope | files touched | est. rows | depends on |
|---|---|---|---:|---|
| **L0 — retirement table** | applies nominations from every lane to `RETIRED_SHADOW_TRIPLES` / `no_image_receiver` | `native-api/src/retired_shadow.rs`, `no_image_receiver.rs` | — | receives from all |
| **L1 — dead surface sweep** | delete the 335 fabricated methods (population E) | `socket_channel.rs`, `nio_native.rs`, `unsafe_natives*.rs`, `foreign_ffm.rs` | 335 | none — **start here** |
| **L2 — `java.util.logging`** | finish the retirement already in flight | `logging_shims.rs`, `logmanager.rs` | 90 stubs + 22 (C) | L0 |
| **L3 — third-party shims** | netty/tcnative, slf4j/jboss, Quarkus, Spring bootstrap | `messaging_shims.rs`, `spring_startup_bootstrap.rs`, `quarkus_staticinit.rs`, `cglib_enhancer.rs` | 228 | needs the libraries' jars |
| **L4 — `j.u.c` locks + latches + `Collections.synchronized*`** | `StampedLock`, `CountDownLatch`, `CyclicBarrier`, atomics, the synchronized wrappers | `util_concurrent_ext.rs`, `atomic_updater.rs`, `phases_late/concurrent.rs`, the `Collections$Synchronized*` rows in `native-collections/src/lib.rs` | 91 stubs + 199 (C) | L0; §5.2 green in strict and **broken in compatible** — highest value per row |
| **L5 — collections (C)** | the 563 `native-collections` bridges that shadow bytecode | `native-collections/src/lib.rs` | 563 | **highest risk**, highest call volume |
| **L6 — `java.lang` core (C)** | `Object`, `Class`, `Enum`, `String`, `StringBuilder` | `native-builtins/src/lib.rs`, `lang_string.rs`, `lang_math.rs` | 925 | **partly immovable** (§4.2) |
| **L7 — NIO / filesystem** | `java.nio.file`, `sun.nio.ch`, channels | `phases_late/nio_file.rs`, `native-io/src/nio_native.rs`, `watch.rs` | 41 stubs + 216 (C) | none |
| **L8 — JCA / TLS** | ciphers, providers, SSL | `jca/*`, `t27_tls.rs`, `phases_late/ssl_security.rs`, `keystore.rs` | 24 stubs + 129 (C) | none |
| **L9 — process / `java.lang` process** | `ProcessBuilder`, `ProcessHandle` | `native-io/src/process.rs`, `phases_late.rs` | 28 | none |
| **L10 — `java.time`, `java.net` codecs, `StringJoiner`** | small, all-green families | `deprecated_io_util.rs`, `deprecated_util.rs`, `deprecated_lang.rs` | 74 | L0 |
| **L11 — foreign / FFM** | `java.lang.foreign`, `jdk.internal.foreign` | `phases_late/foreign_ffm.rs` | 159 (C) | none |
| **L12 — AWT / Swing / 2D** | `native-awt` | `native-awt/src/natives.rs` | 103 (C) | none |
| **L13 — JMX / management** | `javax.management`, `sun.management` | `jmx.rs`, `jmx_openmbean.rs` | 12 stubs + 65 (C) | feature-gated |
| **L14 — category (D) deletion** | the 282 synthetic-exclusive registrars | `phases_early.rs`, `phases_late/*`, `vector_api.rs`, `serialization.rs`, `tls.rs` | ≈4390 call sites | **must diff against the 428 shared registrars first** (§3.1) |

**Safe to run fully in parallel right now:** L1, L7, L8, L9, L11, L12, L13 — no
shared registrar file, no shared table.
**Serialise through L0:** L2, L4, L10.
**Do last, alone:** L5 and L6 — they own the two largest files in the tree
(`native-collections/src/lib.rs`, `native-builtins/src/lib.rs`) and every other
lane's merge conflicts land there. **L14 also touches `native-builtins/src/lib.rs`**
(the 2,780-line `register_synthetic_overrides` body) and therefore cannot run
concurrently with L6.

Suggested order: **L1 first** (cheapest, collision-free, and it shrinks the
population every later lane has to reason about), then the parallel block, then
L14, then L5/L6 alone.

### 7.1 The gate every lane must run

```
cratonvm --jdk-only --explain-jdk-only --dump-native-registry after.json -cp . Hello
python scripts/jdk-only-kind-map.py       # no row's kind changed silently
python scripts/jdk-only-bridge-ratchet.py # bridge.shadows_bytecode must FALL
```

`bridge.shadows_bytecode` is the roadmap's single scalar. It is **3956** today.

---

## 8. What this census could NOT determine

* **(D)'s NET runtime size.** The source-level size is in §3.1 (≈4390 call sites,
  282 registrars). The *net additional registry entries* need a
  `--features synthetic-jdk` build, which this lane did not make. Command in
  §3.1. **Number deliberately blank** — and the in-tree "~3,100 / ~5,200"
  comment is stale, so it must not be used as a stand-in.
* **True liveness.** `invocations` is per-run. Rows at 0 under these probes are
  not proven dead. Needs a corpus-wide merged census (§1.3).
* **The 818 no-class-in-image stubs** (netty, slf4j, Quarkus, Spring, tcnative,
  CratonVM-internal). Untested — needs those libraries' jars, which this lane
  does not own. They are the **largest untested block in the census** and the
  (A)/(B) split for them is genuinely unknown.
* **Whether each of the 3956 shadowing bridges is CORRECT.** The census proves
  they shadow; it does not adjudicate them. That is 3956 differential tests, not
  a census.
* **Why the compatible `ConcProbe` run stops at the `CyclicBarrier`** (§5.2).
  Two runs stop at the same probe, but the VM's exit code was never captured and
  the one thread death observed may be symptom rather than cause. The
  `synchronizedList` data loss beside it **is** established; this row is not.
* **Deep-path behaviour** for the families §5 marked green: file-backed
  `FileHandler`, `LogManager.readConfiguration`, real child processes, TLS
  handshakes, async channel close.
* **`MethodHandleProxies.asInterfaceInstance` fails under `--jdk-only`** with
  `ClassFormatError: ldc: unsupported constant pool entry type at #26`. This is a
  **class-file parsing gap, not a stub**, and so it is out of this census's
  scope — but it is a hard `--jdk-only` blocker for any application using
  `MethodHandleProxies`, and no stub population contains it. **It needs its own
  record.** Not filed here; this lane writes one document only.

---

## 9. Reproducing this document

```
BIN=<scratchpad>/bin/cratonvm-f8.exe
JDK="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot"

"$JDK/bin/javac" -d census census/Hello.java census/StubProbe.java census/ConcProbe.java

$BIN            --explain-jdk-only --dump-native-registry reg-compat-adj.json -cp census Hello
$BIN --jdk-only --explain-jdk-only --dump-native-registry reg-strict-adj.json -cp census Hello
$BIN --jdk-only --explain-jdk-only --dump-native-registry reg-probe-strict.json -cp census StubProbe

"$JDK/bin/java" -cp census StubProbe ; $BIN -cp census StubProbe ; $BIN --jdk-only -cp census StubProbe
"$JDK/bin/java" -cp census ConcProbe ; $BIN --jdk-only -cp census ConcProbe ; $BIN -cp census ConcProbe
```

`ConcProbe` under compatible mode takes several minutes and does not finish (§5.2);
run it last, and keep stderr — the thread death is only visible there.

Classification of a row, in one predicate — this is the whole method:

```python
im = row["image_declaring_method"]          # requires --explain-jdk-only
row["owns_slot"]                            # else it never dispatches
im["declared"] and im["has_code"] and not im["acc_native"]   # (C): shadows real bytecode
im["declared"] and im["acc_native"]                          # legitimate bridge
im["image_has_class"] and not im["declared"] and not im["inherited_from"]  # (E): fabricated
```

Probe sources and the four census JSONs are in this session's scratchpad under
`census/`; they are inputs, not deliverables, and are not committed.
