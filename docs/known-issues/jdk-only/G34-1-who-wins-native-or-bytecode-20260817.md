# G34-1 — what actually decides whether a registered native or real JDK bytecode runs

**Status:** RULE SETTLED — **MEASURED**, on a real binary against a real oracle,
in both directions, cold and warm. The hazard fix in §5 is **FIXED-IN-SOURCE,
AFTER NOT MEASURED** (this lane may not build), and §5 says so where it applies.
No prediction in this record is dressed as a result.

**Provenance.** Binary: `C:/craton/target-rel2/release/cratonvm.exe`, built from
`9964ca733`, mtime `2026-08-17 02:32`, re-checked unchanged at the end of the
lane. The older `C:/craton/target-rel/release/cratonvm.exe` (mtime `02:09`,
pre-`G29-1`) is used deliberately in §2 as a second, differently-registered
binary — the two disagree about what is registered on `HttpHeaders`, which is
what makes one row of the experiment possible at all. `C:/craton/target-fcheck/`
is ignored entirely (that build partly failed; its timestamp misrepresents its
contents). Oracle: HotSpot 25.0.3+9-LTS at
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`. Probes written for
this lane, in `scratchpad/g34/`: `G34Probe.java` (the reader surface on a REAL
JDK-constructed `HttpHeaders`), `G34Probe2.java` (the three disambiguating
signatures), `G34Probe3.java` (300,000 iterations at one call site),
`G34Probe4.java` (all five readers on a CratonVM-MINTED receiver, cold and
warm). Vectors from `C:/craton/cvm-mergecheck/regression-suite/build`.

Files changed: `native-builtins/src/http2.rs`,
`vm/src/runtime/interpreter/native_override.rs`. Nothing else.

---

## 0. The headline — the rule, in one paragraph

**Under `--jdk-only`, registering a `Bridge` for a `(class, name, descriptor)`
triple is by itself sufficient for it to preempt real JDK bytecode.**
`force_native_over_real_jdk_bytecode` is *not* the gate that decides this, and a
class does not need to be on it. The decision is taken at the FIRST dispatch
site that answers, and for nearly every call in the VM that site is
`try_stackless_invoke` step 1 → `resolve_step1_native`
(`vm/src/runtime/interpreter/native_override.rs:7290`) →
`resolve_native_dispatch_wave1` (`vm/src/vm/vm_exec.rs`). Step 1 runs *before*
method resolution, so it passes `compat_native_wins: true` unconditionally and
`bytecode_available: shadows_bytecode && enforce` — and `enforce` is
`env_cache::jdk_only_enforce_shadow_for(class_name)`, which is **off unless
`CRATONVM_ENFORCE_NATIVE_SHADOW` is set**. With it off (the default),
`bytecode_available` is `false`, so `resolve_native_dispatch_wave1`'s
`NativeKind::Bridge` arm returns `Some(NativeBridge(callback))` and the native
runs. The existence of real bytecode buys exactly one thing: a `#[cold]`
observation, `record_native_shadow_ran_over_bytecode`, into the §1.4 shadow
census. It changes no dispatch.

This is not written down anywhere else in this directory, and at least four
lanes have guessed at it. `G29-1` §6 named it "the one thing most likely to be
wrong", and its worry was unfounded — see §3.

### The rule as a decision table

| kind registered | real bytecode? | who runs, `--jdk-only`, default | where decided |
|---|---|---|---|
| `Intrinsic` | either | the **native** | `resolve_dispatch` step 2 / wave1 `Intrinsic` arm |
| `Bridge` | no `Code` | the **native** | documented deviation between steps 3 and 4 |
| `Bridge` | has `Code`, reached via step 1 | the **native** | wave1 `Bridge` arm, `bytecode_available = false` |
| `Bridge` | has `Code`, reached with a resolved `&Method` | the **bytecode** | `resolve_dispatch` step 3 |
| `SyntheticStub` | either | **refused** (§1.3) | both sites |
| anything, `CRATONVM_ENFORCE_NATIVE_SHADOW` armed for the prefix | has `Code` | the **bytecode** | wave1, `bytecode_available = true` |

Two rows of that table fire in the SAME run, for the SAME triple, from
different sites — §4 measures it. That is the part a one-row experiment cannot
see, and it is why the answer to "does my native run" is *site*-dependent rather
than *triple*-dependent.

### Where `force_native_over_real_jdk_bytecode` actually sits

It is a **second, later** gate consulted only by the sites that already resolved
a bytecode `Method` **without asking the registry** — the vtable inline-cache
(`dispatch_virtual.rs:767` and `:3432`) and the JIT (`jit_bridge.rs:2748`).
There it converts a would-be `CachedInvokeTarget::Bytecode` entry into a
`VirtualNative` one. It is a **cache-shape override**, not the mode's policy.
That is why it can be simultaneously true that (a) the list is short and does
not contain `HttpHeaders` or `Optional`, and (b) natives on both of those
classes run.

The doc banner on that function said, in its own words, that under `--jdk-only`
every branch is dead because step 3 returns `Bytecode` for anything with `Code`.
That sentence is true *of `resolve_dispatch`* and false as a claim about the
mode, because step 1 answers first. §5.2 corrects it in place. This is the
handoff's "comments actively lie about it" trap, and it is the specific comment.

---

## 1. Why this needed an experiment and not a reading

`G29-1` registered four new readers on `java/net/http/HttpHeaders` and could not
tell whether they would ever run. Its evidence that natives win anyway was
indirect and it said so: `java/util/Optional` is also absent from the force
list, also has `real_declaring_method.has_code = true`, and showed
`invocations=244`. That is an inference from one family.

It is also an inference from an instrument this directory has since measured to
be unreliable in magnitude (`G33-1`: 100,000 `Math.abs` calls report `1`), so
`invocations` can only be read as a boolean. A boolean from one family is not a
rule.

The decisive experiment is cheap and nobody had run it.

---

## 2. MEASURED — three independent signatures on a REAL JDK receiver

The experiment needs a receiver the **real JDK** built, so that the real body
and the CratonVM native would give visibly different answers. `HttpHeaders.of
(Map, BiPredicate)` is concrete JDK bytecode with no registration on either
binary, so the object it returns has the real class's layout.

From the JDK's own sources (`$JAVA_HOME/lib/src.zip`,
`java.net.http/java/net/http/HttpHeaders.java` — readable, and worth reading):
`of(...)` stores `unmodifiableMap(other)` into the single `headers` field and
`map()` returns that field. So the real body's answer is a **stored, shared,
unmodifiable** map. CratonVM's registered `map()` native
(`net_phase_e.rs:13603`) mints a **fresh, private, mutable
`java/util/LinkedHashMap`** on every call. Those differ on three axes at once,
and no one axis alone would settle it.

`G34Probe2.java`, both VMs, headers `{Accept: [text/plain, text/html], X-Num:
[42]}`:

| signature | HotSpot | CratonVM `--jdk-only` | says |
|---|---|---|---|
| `h.map().getClass().getName()` | `java.util.Collections$UnmodifiableMap` | **`java.util.LinkedHashMap`** | the native minted it |
| `h.map() == h.map()` | `true` | **`false`** | the native minted a NEW one per call |
| `h.map().put(...)` | `UnsupportedOperationException` | **ACCEPTED** | it is the native's mutable map |
| `filterCalls` (control) | `3` | `3` | the real `of()` body ran on both |

All three agree, and the control confirms the real constructor bytecode did run
— so this is not "CratonVM never built the object".

`java/net/http/HttpHeaders` is **not** in `force_native_over_real_jdk_bytecode`
(verified across the whole function body, lines 2524–4989: the only occurrence
of the string is an unrelated comment about Spring's same-named class; the only
occurrence of `Optional` is the descriptor of `Runtime$Version.build()`).

**The VM says so itself.** `--jdk-only-report` writes a machine-readable
violation list, and the `bridge-ran-over-bytecode` tag is emitted from exactly
one place in the tree — `record_native_shadow_ran_over_bytecode`, called only
from `resolve_step1_native`. So the tag does not merely say "a native won", it
names the site:

```json
{"kind": "native-shadows-bytecode",
 "summary": "bridge-ran-over-bytecode native shadows bytecode of java/net/http/HttpHeaders.map()Ljava/util/Map;",
 "class": "java/net/http/HttpHeaders", "method": "map",
 "descriptor": "()Ljava/util/Map;", "native_kind": "bridge-ran-over-bytecode"}
```

That is the whole rule, instrumented, in the VM's own output. **Use
`--jdk-only-report <FILE>` for this question.** It was not mentioned in the
handoff and it answers directly what `--dump-native-registry` only implies.

### 2.1 Stable across call-site warmup — which is where the force list lives

The force list is consulted by the inline-cache and JIT paths, so the obvious
way for the rule to be half-true is for a cold call to take the native and a
warm one to take the cached bytecode. `G34Probe3.java` puts `h.map().getClass()`
at ONE call site and runs it 300,000 times:

| iteration | HotSpot | CratonVM |
|---|---|---|
| 0 | `Collections$UnmodifiableMap` | `LinkedHashMap` |
| 1 | `Collections$UnmodifiableMap` | `LinkedHashMap` |
| 100 | `Collections$UnmodifiableMap` | `LinkedHashMap` |
| 20,000 | `Collections$UnmodifiableMap` | `LinkedHashMap` |
| 299,999 | `Collections$UnmodifiableMap` | `LinkedHashMap` |

No flip, at any point, in either direction. The probe also watches for a change
between consecutive iterations and never reports one. **Cold and warm agree
without a force-list entry.**

---

## 3. MEASURED — `G29-1`'s N2 is answered: no force-list entry is needed

`G29-1` N2 asked for exactly one measurement before adding
`java/net/http/HttpHeaders` to the force list. Taken, on `target-rel2`, which is
the first binary that carries `G29-1`'s five readers. `G34Probe4.java` uses a
CratonVM-**minted** receiver — `HttpRequest.newBuilder(u).header(..).build()
.headers()` — whose slot 0 holds a `String[]` where the real class declares a
`Map`. If the real bodies won, every reader would read that `String[]` as a
`Map`.

| call | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `h.map()` | `{Accept=[text/plain, text/html], X-Num=[42]}` | **identical** |
| `h.firstValue("accept")` | `Optional[text/plain]` | **identical** |
| `h.allValues("Accept")` | `[text/plain, text/html]` | **identical** |
| `h.firstValueAsLong("X-Num")` | `OptionalLong[42]` | **identical** |
| `h.toString()` | `java.net.http.HttpHeaders@X { {Accept=[…], X-Num=[42]} }` | **identical** |
| the first three again, after 300,000 warm iterations | as above | **identical** |
| `h.map().getClass()` | `java.util.Collections$UnmodifiableMap` | `java.util.LinkedHashMap` |

Registry rows for the class on that binary, all five `owns_slot=true`,
`overwrote=null`, `real_declaring_method.has_code=true`:

```text
map()Ljava/util/Map;                                    net_phase_e.rs:13603  invocations=300003
firstValue(Ljava/lang/String;)Ljava/util/Optional;      net_phase_e.rs:13679  invocations=300002
allValues(Ljava/lang/String;)Ljava/util/List;           net_phase_e.rs:13704  invocations=300002
firstValueAsLong(…)Ljava/util/OptionalLong;             net_phase_e.rs:13737  invocations=1
toString()Ljava/lang/String;                            net_phase_e.rs:13777  invocations=1
```

and all five tagged `bridge-ran-over-bytecode` in `--jdk-only-report`.

**Verdict: do NOT add `java/net/http/HttpHeaders` to
`force_native_over_real_jdk_bytecode`.** It would not make a non-running native
run; it would change which body real `HttpHeaders` receivers get on warm call
sites, for every receiver of a real, widely-used JDK class. That is precisely
the failure the `java/lang/String` arm was deleted for on 2026-08-04 (a method's
behaviour began depending on how many times its call site had executed) and the
`ThreadPoolExecutor` arm on 2026-08-06. §5.2 records the absence as deliberate,
with a test, so the next lane does not "fix" it.

`G29-1`'s §6 fallback — "store a real case-insensitive unmodifiable `Map` in
slot 0" — is therefore not needed for correctness of the five readers. It would
still close the one remaining divergence in the table above; nominated as **N3**.

The same run gives `G29-1`'s §6 prediction its measurement, which that record
was explicitly not allowed to claim: **`RJdkOptionalShape` is `checks=1418
PASS`** on `target-rel2`, up from `process=172` + `AbstractMethodError`. All of
§6's predicted order of falling assertions is moot — they all fell.

---

## 4. MEASURED — the other direction, and why the rule is site-shaped

The same `--jdk-only-report` from a trivial 40-line probe run carries **69**
`native-shadows-bytecode` violations in two flavours, and the counters name them:
`interpreter_shadow_unenforced=45` (a bridge RAN over bytecode) and
`interpreter_bytecode_preferred=52` (bytecode won over a bridge).

45 distinct triples had a bridge win over real bytecode. A sample, none of them
on the force list:

```text
java/io/PrintStream.println(Ljava/lang/String;)V      java/util/Arrays.asList([Ljava/lang/Object;)Ljava/util/List;
java/lang/Object.<init>()V                            java/util/HashSet.iterator()Ljava/util/Iterator;
java/lang/Class.getName()Ljava/lang/String;           java/net/http/HttpHeaders.map()Ljava/util/Map;
java/lang/StringBuilder.append(C)…                    java/util/AbstractMap$SimpleEntry.getKey()…
```

21 triples went the other way. **Ten of them appear in BOTH lists in the same
run** — `java/lang/Class.getName`, all four `StringBuilder` rows, four
`ArrayList` rows, `HashMap$KeyIterator.hasNext`/`next`. So:

> The outcome is a property of the **dispatch site**, not of the triple. The
> same `(class, name, descriptor)` can take the native from step 1 and the
> bytecode from a site that arrived with a resolved `&Method`, in one process,
> minutes apart.

That is the fact that makes `force_native_over_real_jdk_bytecode` coherent: it
exists to make the *later* sites agree with step 1 for families where the native
must win because it services a receiver the JDK body cannot (a `Map.values()`
view minted as an `ArrayList` that must re-sync on read). It is not, and has
never been, the switch that turns natives on.

Never generalise this to "the list is unnecessary". It is necessary for exactly
the families whose comments say why, and this record removes none of them.

### 4.1 The dial, for whoever wants the strict behaviour

`CRATONVM_ENFORCE_NATIVE_SHADOW` arms enforcement at step 1. It accepts
`1`/`all` or a comma-separated list of **internal class-name prefixes**
(`java/util/logging/,javax/management/`), so a subsystem can be migrated alone.
Default off, and that is a measurement, not a preference: arming it whole-corpus
took `--jdk-only` from **32 passed / 17 failed to 3 passed / 46 failed**
(Azure Linux, JDK 25, 2026-08-06). The failures are not dispatch faults — under
`--jdk-only` the surviving bridges ARE the object model for large parts of
`java.base`, so yielding them to bytecode hands real code objects it cannot
service. All five blocker families are in
`jdk-only-step1-bytecode-available-RESOLVED-20260806.md`.

---

## 5. What changed

### 5.1 `native-builtins/src/http2.rs` — the last-write-wins layout hazard (`G29-1` N1)

**Deletion was the first option and it is wrong, and this is why.** `G29-1`
recorded that `register_http2_natives` "is not on this boot path". That is true
of `--jdk-only` and it is not the same as unreachable: it is called from
`lib.rs:24865`, inside `#[cfg(feature = "synthetic-jdk")]
register_synthetic_overrides`, which `register_builtins` calls. Deleting the
registrations would remove synthetic-jdk mode's `HttpHeaders` surface outright.
This branch has already shipped a fix into dead code once (`8c72d23ca`); it did
not need to also delete live code for being quiet.

**The hazard is worse than "one boot-order change away", and that is the
finding.** In any build with `synthetic-jdk` enabled, BOTH registrars run and
the order is fixed by `register_builtins`:

```text
register_builtins
  ├─ register_essential_natives …            → net_phase_e::register_phase_e_networking   (lib.rs:18765)
  │                                             registers map/firstValue/allValues/
  │                                             firstValueAsLong/toString  — 1-slot String[] shape
  └─ register_synthetic_overrides            → register_http2_natives                     (lib.rs:24865)
                                                re-registers FOUR of those five           — 3-Int-counter shape
```

`register()` is last-write-wins, so http2's four bodies own the slots while
`net_phase_e`'s minter keeps producing `String[]`-shaped receivers for them —
and `net_phase_e`'s `toString` survives, because http2 does not register it. The
result is a **mixed decoder set on one class**: four readers decoding counters,
one decoding a `String[]`, over objects from two different minters. That is
strictly worse than either registrar alone.

This is **INFERRED FROM SOURCE, NOT MEASURED.** The binaries available to this
lane are `--jdk-only`/real-JDK builds in which `register_synthetic_overrides`
does not run at all — confirmed, not assumed: `--dump-native-registry` carries
exactly one `java/net/http/HttpHeaders` row per method and every one is
`net_phase_e`'s with `overwrote=null`. No probe here can reach the synthetic
path, so the paragraph above is a reading of `lib.rs`, and the next lane that
can build a `synthetic-jdk` binary should confirm it with `--dump-native-registry`
(landed when every `HttpHeaders` row reports one `registered_by` and
`overwrote=null`).

**What was done instead: a receiver-shape guard, which is the file's own
existing pattern applied to the four accessors that were missing it.**
`register_http_headers`'s `<init>` already guards with `is_synthetic_shape`; the
four accessors did not. But `is_synthetic_shape` compares the class NAME, and
all three candidates answer to `java/net/http/HttpHeaders`, so it cannot
separate them. The discriminator that can is the KIND of slot 0 — a property of
the three minters rather than of a name list, which is what makes it hold when a
fourth minter appears:

| minted by | slots | slot 0 holds |
|---|---|---|
| `http2.rs` `alloc_http_headers` | 3 | `Value::Int` — `HDR_COUNT` |
| `net_phase_e::re5_make_http_headers` | 1 | `Value::Object` — a `String[]` of `"k: v"` |
| the real JDK's `HttpHeaders.of(Map, BiPredicate)` | the real class's | `Value::Object` — a real `Map` |

New `http_headers_is_counter_shape`, consulted by `allValues`, `firstValue` and
`firstValueAsLong`. `map()` is guarded more strongly still — it now reads no
counter slot at all (see below).

**The guard is tested BEFORE the slot reads, not folded into a match arm after
them.** `net_phase_e`'s receiver has exactly ONE slot, so reading `HDR_HAS_CT`
(slot 1) and `HDR_HAS_CL` (slot 2) off it is an out-of-range field access, and
the first version of this fix — `Value::Int(n) if owned => n` — had already
taken it before the guard could fire. That is recorded because it is the whole
class of bug this record is about, reintroduced by the fix for it.

**Two crate-convention divergences found by the sweep the assignment asked
for**, both in the objects `register_http_headers` HANDS BACK rather than in the
one it reads:

* `map()` allocated `java/util/HashMap` with **2** slots. Every other
  `java/util/HashMap` allocation in `native-builtins` — `phases_early.rs:393`,
  `logging_shims.rs:3516`, `phases_late.rs:1550`, `reflect_annotations.rs:997`,
  `locale_resources.rs:902`, `jmx_openmbean.rs:1586` and one more — asks for
  **3** and pairs it with `native_map_init`. `http2.rs` was the only 2-slot
  allocation of the class in the crate. It then hand-wrote `Int(count)` into
  slot 0, so the map it returned could not be read by any registered
  `native_map_*` body: it claimed a size in a slot the layout does not keep a
  size in, over entries it never had. Now 3 slots + `native_map_init`. An
  honestly-empty well-formed map beats a size no `get` can honour — and the
  counters carry no header NAMES, so there was never anything to put.
* `allValues()` hand-wrote `Int(count)` into slot 0 of a `java/util/ArrayList`,
  which is where `native_al_init` keeps the backing `Object[]` — a type-punned
  slot (the `W7-84` autobox shape), and a list that reported a size it could not
  produce an element for. Now `native_al_init` + `native_al_add` of the actual
  value, pinned across both allocations the way
  `HttpRequest$Builder.version` in the same file already pins, with the pin
  released before any `?` so an error cannot leak it.

The full sweep for "a class `http2.rs` allocates with a slot count that
disagrees with another file's allocation of the same class" is §6.

**AFTER NOT MEASURED.** These are source changes and this lane may not build.
The `--jdk-only` behaviour of the eight vectors in §7 is unchanged *because the
binary is unchanged*, which is exactly what a source-only change must produce
and is evidence about nothing else.

### 5.2 `vm/src/runtime/interpreter/native_override.rs` — the rule, written down where it is looked for

No force-list entry was added, because §3 measured that none is needed. What
was added is the thing this lane was asked for: the rule, at the point where the
next lane will look for it.

* The banner on `force_native_over_real_jdk_bytecode` now states what the
  function does and does **not** decide, names `resolve_step1_native` and the
  `CRATONVM_ENFORCE_NATIVE_SHADOW` dial as the actual mechanism, carries §2 and
  §3's measurements, and states the blast radius of adding a real,
  widely-used JDK class. The pre-existing sentence "under `--jdk-only` every
  branch is dead" is left standing as the true statement about `resolve_dispatch`
  that it is, immediately followed by why it is not a statement about the mode.
* Three tests in a new `force_list_deliberate_absences_tests` module assert that
  the five `HttpHeaders` readers and five `Optional` methods are **absent**, with
  the measurement in the doc comment and an explanation of what a failure would
  really mean. A test for things that are not there earns its place here because
  the natural "fix" for a misread of this area is to add them.
* A negative control (`the_list_is_not_vacuously_empty`) pins three entries that
  ARE on the list, so the two absence tests cannot pass by the function having
  been emptied.

### 5.3 Tests

Five new tests in `http2.rs`'s existing `mod http2_tests`: the three-minter
discrimination, `firstValue` and `firstValueAsLong` declining a foreign receiver
while still answering this file's own, a source-level ratchet that fails if any
accessor loses its guard, and one that pins the two collection conventions of
§5.1. Three new tests in `native_override.rs` as described above.

The two source-level ratchets are source-scanning on purpose: the failure being
guarded is "someone edits this function and forgets", which no amount of
behaviour on today's receivers can catch. There is precedent in the tree
(`native_override.rs`'s own `assert!(!code.starts_with(...))` guard and
`native-api/tests/guarded_slot_maps.rs`).

---

## 6. The `http2.rs` allocation sweep, MEASURED by grep over `native-builtins/src`

Every class `http2.rs` allocates, against every other allocation of the same
class in the crate. `=` means no disagreement.

| class | `http2.rs` | elsewhere | assessment |
|---|---|---|---|
| `java/net/http/HttpHeaders` | 3 | 1 (`net_phase_e`) | **the hazard** — §5.1. `http_client.rs`'s former 2-slot allocation is gone; `G29-1` routed it through `re5_make_http_headers`, so this is down from three shapes to two |
| `java/util/HashMap` | 2 → **3** | 3 (×6), 8 (`spring_startup_bootstrap`) | **fixed** — was the crate's only 2-slot allocation |
| `java/util/ArrayList` | 2 | 1, 2 (×6), 4 | count agrees with the majority; the **initialisation** did not — fixed |
| `java/net/http/HttpResponse` | 7 | 3 (`servlet.rs:8417`) | disagrees. `http2.rs` registers `register_http_response`; `servlet.rs` does not register accessors on the class — **N1** |
| `javax/net/ssl/SSLSession` | 6 | 4 (`t27_tls.rs`), 6 (`tls.rs`) | `tls.rs` agrees, `t27_tls.rs` does not — **N2** |
| `javax/net/ssl/SSLContext` | 4 | 1, 2, 2, 12 | four-way disagreement across five files — **N2** |
| `java/net/URI` | 2 | 1, 6, 7, 18 | five-way. `http2.rs` registers no `java/net/URI` natives, so its 2-slot object is only ever read by other files' decoders — **N2** |
| `java/util/Optional` | 1 | — | agrees with `D3-2`/`E2-1`: slot 0 is the reference `value` |
| `java/util/OptionalLong` | 2 | — | correct and deliberately different — the real layout IS `(boolean, long)` |
| `java/net/http/HttpClient`, `…$Builder`, `HttpRequest`, `…$Builder`, `…$BodyPublisher`, `HttpResponse$BodyHandler`, `WebSocket`, `…$Builder`, `CompletableFuture`, `SSLParameters` | — | no second allocator, or no registered decoder | `=` |

Only the first three were in scope for this lane's files AND actionable without
a build. The rest are nominated rather than touched: changing a slot count is a
change to what every reader of that class decodes, and this lane can measure
none of those paths.

---

## 7. Baselines, MEASURED on `target-rel2`, before and after the edits

All eight run with `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 … --jdk-only`, vectors
from `C:/craton/cvm-mergecheck/regression-suite/build`. `native_override.rs` is
on the dispatch path for every call in the VM, which is why the last three are
here.

| vector | before edits | after edits |
|---|---|---|
| `RJdkOptionalShape` | `PASS`, `checks=1418` | `PASS`, `checks=1418` |
| `RJdkNet` | `PASS`, `checks=81` | `PASS`, `checks=81` |
| `RJdkHello` | `PASS`, `checks=41` | `PASS`, `checks=41` |
| `RCrypto` | `PASS`, `checks=57` | `PASS`, `checks=57` |
| `RJdkAsyncChannel` | `PASS`, `checks=141` | `PASS`, `checks=141` |
| `RJdkCollections` | `PASS`, `checks=69` | `PASS`, `checks=69` |
| `RCollections` | `PASS`, `checks=53` | `PASS`, `checks=53` |
| `RStrings` | `PASS`, `checks=46` | `PASS`, `checks=46` |

**No number in the "after" column is evidence about the fix.** The binary is
byte-identical across both columns (mtime `2026-08-17 02:32`, re-checked); a
source-only change against an unrebuilt binary must produce exactly this, and
the column exists to show the binary did not change under the lane. The next
lane to build should re-run all eight.

`rustfmt --edition 2021 --check` was run **in place, in its tree** on both files
(not on a copy — a copy makes rustfmt abort on unresolvable `mod`s and report
success while writing nothing, which reads as a pass). `http2.rs` reports **29**
diffs against **29** on the `HEAD` blob measured the same way; `native_override
.rs` reports **10** against **10**. **No new hunk in either file.** One new hunk
did appear on the first pass, in an added test, and was fixed. That is a parse
check, not a type check.

---

## 8. What this lane did NOT do

* **It did not build the binary.** §§0–4, 6 and 7 are MEASURED on
  `target-rel2`; §5's changes are PREDICTED to compile and are not measured.
* **It did not measure the `synthetic-jdk` path at all**, which is the only path
  on which the §5.1 hazard is live. Everything said about registrar ORDER there
  is a reading of `lib.rs`, flagged as such in §5.1.
* **It did not delete the `http2.rs` registrations**, and §5.1 gives the
  reachability proof for why not — the opposite of the failure `8c72d23ca`
  recorded.
* **It did not route `http2.rs`'s `alloc_http_headers` through
  `re5_make_http_headers`.** That is the "one minter for one class" end state and
  it is the right one, but it changes what every `http2.rs` accessor decodes on a
  path this lane cannot run. **N1.**
* **It did not add anything to `force_native_over_real_jdk_bytecode`**, and §3 is
  the measurement that says it must not.
* **It did not settle whether the JIT's own gate can diverge from step 1 on a
  triple step 1 never sees.** `jit_direct_native_binds`,
  `jit_inline_cache_natives` and `jit_fastpath_admissions` were all **0** in
  every run here, including the 300,000-iteration one, so the JIT arm of the
  rule is UNEXERCISED rather than confirmed. A vector that drives a forced
  triple through a JIT-compiled call site would close it. **N4.**
* **It did not touch `net_phase_e.rs`, `http_client.rs`,
  `http_url_connection.rs`, `lang_invoke.rs`, `lib.rs`, `INDEX.md` or
  `README.md`.**
* **It ran no state-changing git command and no `cargo` command**, and did not
  run `regression-suite/run.sh`.

---

## 9. Nominations

**N1 — `native-builtins/src/http2.rs`'s minter, and `servlet.rs`'s
`HttpResponse` (both outside what this lane could measure).** The end state for
`java/net/http/HttpHeaders` is one minter, not two: `alloc_http_headers` should
build the `"k: v"` `String[]` and call
`crate::net_phase_e::re5_make_http_headers`, exactly as `http_client.rs:1708`
and `:1854` already do, after which `http2.rs`'s four accessor registrations
become redundant with `net_phase_e`'s five and can be deleted with a
reachability proof rather than a guard. That is a behaviour change on the
synthetic-jdk path, so it needs a `synthetic-jdk` build to verify — which is the
only reason it is nominated rather than done. Separately, `servlet.rs:8417`
allocates `java/net/http/HttpResponse` with **3** slots where `http2.rs:663`
allocates **7** and registers the decoders; verify with
`--dump-native-registry` plus a probe that reaches a `servlet.rs`-minted
response.

**N2 — the SSL and URI slot-count spreads (`t27_tls.rs`, `tls.rs`,
`net_phase_e.rs`, `http_client.rs`, `http2.rs`).** §6 measures
`javax/net/ssl/SSLContext` allocated at **1, 2, 2, 4 and 12** slots across five
files, `javax/net/ssl/SSLSession` at **4 and 6**, and `java/net/URI` at **1, 2,
6, 7 and 18**. Each is the `HttpHeaders` hazard's shape, at greater width and
with more registrars in play — and `F18-1`/`F21-1`/`E22-1` have already recorded
session-accessor defects in that family. The tractable first step is the one
`G29-1` took for `HttpHeaders`: find the minter each decoder actually pairs with,
name it, and route the others through it. Do not start by unifying the counts.

**N3 — `net_phase_e.rs`, the last `HttpHeaders` divergence.** After §3, exactly
one row of the reader surface still differs from the oracle:
`headers().map().getClass()` is `java.util.LinkedHashMap` where HotSpot says
`java.util.Collections$UnmodifiableMap`, and the returned map is mutable and
freshly minted per call where HotSpot's is shared and unmodifiable
(`map() == map()` is `false` here, `true` there). `G29-1` §6 already described
the fix — store a real case-insensitive unmodifiable `Map` in slot 0 — and noted
it would additionally make every real JDK body correct on these receivers. It is
now a small, well-scoped divergence rather than a contingency plan.

**N4 — a JIT-path vector for the dispatch rule.** §8 records that all three JIT
refusal counters were 0 in every run, so §0's table is measured for the
interpreter and the inline cache but only *reasoned* for the JIT
(`jit_bridge.rs:2748`). A vector that drives a forced triple — e.g.
`java/util/ArrayList.size()I` on a `Map.values()` view — through a JIT-compiled
call site and dumps `--jdk-only-report` would either confirm the row or find the
one place the rule differs.

**N5 — `HttpHeaders.of(Map, BiPredicate)` loses its entries.** MEASURED on the
pre-`G29-1` binary `target-rel`, where `firstValue`/`allValues`/
`firstValueAsLong` had NO registration and therefore ran real JDK bytecode:
`HttpHeaders.of(m, (a,b) -> true)` with a two-entry `LinkedHashMap` answers
`firstValue("accept") = Optional.empty` and `allValues("Accept") = []`, against
`Optional[text/plain]` and `[text/plain, text/html]` on HotSpot — while the
filter is invoked 3 times on BOTH VMs. So `of()`'s body runs and its
`TreeMap`-building loop drops every entry. On `target-rel2` this is invisible
(the new natives answer instead), which is why it is recorded here from the
older binary and why it should be probed with the natives out of the way. Likely
in the `forEach`/`put` path over a real `LinkedHashMap`, which §4 shows is one
of the triples where bytecode and native BOTH ran in one process.

**N6 — this directory's index and `BASELINE-20260817.md`.** `G29-1` is marked
"AFTER NOT MEASURED"; §3 and §7 here supply the after. Suggested amendments,
neither file being this lane's:

- to `G29-1` §0, appended to the Status block: `**Measured after, by `G34-1`
  (2026-08-17, `target-rel2`/`9964ca733`): `RJdkOptionalShape` is `checks=1418
  PASS`. §6's prediction landed, and its N2 worry is answered — the `HttpHeaders`
  natives DO preempt the real bytecode, with no `force_native_over_real_jdk_bytecode`
  entry.**`
- to `BASELINE-20260817.md`'s Triage table:
  - exact old text: `` | `RJdkOptionalShape` | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` | interface doors | ``
  - exact new text: `` | `RJdkOptionalShape` | — | **GREEN, `checks=1418`** (G13-1 N3 → G29-1 → measured by G34-1) | ``

---

## 10. The one-sentence version

Whether a registered native or real JDK bytecode runs is decided by the first
dispatch site to answer — which for nearly every call is `try_stackless_invoke`
step 1, and step 1 tells the policy resolver `bytecode_available: false` unless
`CRATONVM_ENFORCE_NATIVE_SHADOW` is armed — so under `--jdk-only` a registered
`Bridge` preempts real bytecode by default, on or off
`force_native_over_real_jdk_bytecode`, which turns out to be a warm-call-site
cache-shape override and not the gate anybody thought it was.
