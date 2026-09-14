# G50-1 — two drift families settled: `ByteArrayOutputStream` closed by deletion, `java.time.Instant` closed by adjudication, and the first end-to-end differential of the 16 triples `--jdk-only` refuses

Status: **`G41-1` N2 CLOSED — the twelve `java/io/ByteArrayOutputStream` triples
in `serialization.rs` are deleted, on a fresh two-mode registry dump plus the
call-order proof the dump cannot give. `G41-1` N4 ADJUDICATED, not closed — the
sixteen `java/time/Instant` triples still drift, and falling through to real JDK
bytecode under `--jdk-only` is now MEASURED CORRECT (58 assertions, byte-identical
across HotSpot 25, CratonVM compatible and CratonVM `--jdk-only`), so the defect
in that family is the registration and its name, not the behaviour. Predicted new
strict drift total: 1,244 → 1,232.** Wave G, lane G50, 2026-08-17.

Files changed: `native-builtins/src/serialization.rs`,
`native-builtins/src/util_time.rs`, and this record. Nothing else — the
`native-io`, `lib.rs`, `registrar_drift.rs` and `registrar_reachability.rs`
halves are NOMINATIONS in §6.

---

## 0. What was and was not run

**`cargo` was not available to this lane either.** That is the fourth lane in a
row. `registrar_drift.rs` has *still* never been compiled, and this lane's two
new unit tests in `serialization.rs` have never been compiled either. Say it
plainly: `rustfmt --edition 2021 --check` passing proves the files parse, not
that they type-check.

What *was* run:

* **`cratonvm --dump-native-registry`, in both modes**, from
  `C:/craton/target-rel3/release/cratonvm.exe` (`9ae371468`).
  `target-rel4` does not exist on this machine; `target-fcheck` was ignored per
  instructions. 12,039 registry rows in compatible mode, 10,691 under
  `--jdk-only`.
* **`cratonvm --jdk-only --jdk-only-report`**, 1,461 violations, read for the
  `Instant` rows.
* **A 58-assertion `java.time.Instant` differential** against a HotSpot
  25.0.3+9-LTS oracle, on both CratonVM modes.
* **The nine required regression vectors**, in both modes, diffed byte for byte
  against the same oracle (§5).
* A re-parse of `registrar_drift.rs`'s own `DRIFT_TRIPLES` table, which
  reproduces its three constants exactly (111 passes / 1,380 pairs / 1,244
  triples) — the check that makes §4's arithmetic worth quoting.

Provenance tags: **MEASURED-ON-BINARY** (a dump, a report or a VM run says it),
**SOURCE-VERIFIED** (a human read the lines and they say what is claimed),
**PREDICTED** (neither).

---

## 1. The headline

| | before | after |
|---|---|---|
| `ByteArrayOutputStream` drift (`G41-1` N2) | 12 triples, LIVE | **0 — closed by deletion** |
| `java/time/Instant` drift (`G41-1` N4) | 16 triples, undecided | 16 triples, **still drifting; behaviour adjudicated CORRECT** |
| strict drift total | 1,244 | **1,232** (PREDICTED, §4) |
| `(pass, triple)` pairs | 1,380 | 1,368 |
| passes in `DRIFT_TRIPLES` | 111 | 110 |
| lines removed from `serialization.rs` | — | **245** (114 added: 59 doc, 55 test) |
| does `util_time.rs` have any `--jdk-only` reach? | claimed no, 2026-08-11, one mode | **measured no, 2026-08-17, BOTH modes** |

---

## 2. `G41-1` N2 closed: the `ByteArrayOutputStream` twin

### 2.1 What was there — SOURCE-VERIFIED

`serialization.rs`'s `register_byte_array_output_stream` registered exactly 12
triples on one class — `<init>()V`, `<init>(I)V`, `write(I)V`, `write([BII)V`,
`toByteArray()[B`, `size()I`, `reset()V`, `toString()Ljava/lang/String;`,
`toString(Ljava/lang/String;)…`, `toString(Ljava/nio/charset/Charset;)…`,
`flush()V`, `close()V`. No loop, no `format!`, one `let cls = "…"` binding.

Its bodies addressed a two-slot synthetic layout (slot 0 `data:[B`, slot 1
`size:I`), and `flush`/`close` were `|_ctx, _args| Ok(None)` — no-ops, under a
KEEP comment arguing that a no-op *is* the real JDK behaviour.

That comment is right about the JDK and wrong about this VM.
`cratonvm_native_io::native_baos_close` dispatches
`BaosEvent::Close` and then runs `process_pipe_output_close`;
`native_baos_flush` dispatches `BaosEvent::Flush` and then
`ctx.fd_table().flush(fd)`. That is the machinery behind a `Process`'s stdin
pipe, which the VM models as a `ByteArrayOutputStream`. A no-op silently skips
it.

### 2.2 Why the twelve were dead — MEASURED-ON-BINARY, plus the half a dump cannot show

The dump alone cannot settle this family, and saying why is the point of this
section. `register_byte_array_output_stream` is reachable **only** from
`register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]` —
a feature neither `cratonvm-vm` nor `cratonvm-cli` enables. So the shipping
binary this lane dumped **does not contain these twelve registrations at all**,
and no dump taken from it can show them losing a race. Two halves are needed.

**Half one — the dump, on the shipping binary, in both modes.**
`cratonvm --dump-native-registry`, `9ae371468`, 2026-08-17:

| `java/io/ByteArrayOutputStream` | compatible | `--jdk-only` |
|---|---|---|
| rows | 13 | 13 |
| `kind` | `bridge` ×13 | `bridge` ×13 |
| `owns_slot` | `true` ×13 | `true` ×13 |
| `overwrote` | `null` ×13 | `null` ×13 |
| `registered_by` | `native-io/src/lib.rs:6871`–`:6898` | identical |
| rows naming `serialization.rs` | **0** of 12,039 | **0** of 10,691 |

Five rows carry `invocations > 0` — `write([BII)V` 77, `flush()V` 14,
`<init>()V` 11, `toByteArray()[B` 11, `close()V` 7 — taken on an `RSerial` run,
which is positive proof the native-io bodies are the ones that run.
`invocations == 0` on the other eight is used as evidence for **nothing**: four
dispatch families bypass the counter and it is exact only under `--nojit` with
`CRATONVM_DISABLE_INTRINSICS=1` (`G33-1`). `owns_slot` is what was read.

**Half two — the call order, for the build the dump cannot reach.**
SOURCE-VERIFIED:

* `vm/src/vm/vm_init.rs`, the `#[cfg(feature = "synthetic-jdk")]` +
  `config.use_synthetic_jdk` arm, is the ONLY arm that reaches this pass. Its
  first two statements are `register_builtins(&mut native_methods);` then
  `register_io_natives(&mut native_methods);`, in that order and adjacent.
* `register_builtins` is `register_essential_natives` followed by
  `register_synthetic_overrides`, which is where both call sites of
  `register_byte_array_output_stream` live (`lib.rs`'s direct call, and
  `register_serialization_natives`).
* `NativeMethodRegistry::register` is last-registration-wins **on the
  callback**: `native-api/src/registry.rs`'s `Some(idx)` arm assigns
  `slot.callback = callback` in place, precisely so an already-issued
  `NativeMethodId` stays valid and picks up the new body. The kind may be
  preserved from an earlier *chosen* category; the callback never is.
* `register_io_natives` binds thirteen `java/io/ByteArrayOutputStream` triples
  from a `let baos = "java/io/ByteArrayOutputStream";` block, unguarded — no
  `#[cfg]`, no `drops_real_layout_synthetic()` test, unlike the
  `InputStreamReader` block twenty lines above it.

So in the one build where these twelve were ever compiled, they were replaced
before any bytecode ran. In every other build they were not compiled. There is
no configuration in which they answered a call.

### 2.3 `F34-1` §5's trap does not apply

The trap is that deleting a synthetic-only pass can take away triples its twin
never registered. native-io's set is a strict **superset**: thirteen triples to
these twelve, the extra being `write([B)V`, which was never here. Nothing is
lost.

### 2.4 What was changed

`register_byte_array_output_stream` is now an empty, heavily documented
function, and the five helpers it exclusively owned — `baos_buffer_bytes`,
`baos_charset_key`, `decode_utf16_bytes`, `decode_baos_bytes`,
`charset_object_name` — are **deleted**, not left behind: 245 lines gone.
Leaving them would have left a plausible-looking, unreachable
`ByteArrayOutputStream` family for the next lane to "fix", and *this branch has
already shipped a fix into dead code once* (`8c72d23ca`).

The function itself is kept because its two callers are in
`native-builtins/src/lib.rs`, which this lane does not own. Deleting the call
sites is nomination **N1** below.

Every symbol removed was grepped across all seven crates plus `vm-cli`,
excluding `target/` and `scratch*/`: `baos_buffer_bytes`, `baos_charset_key`,
`decode_baos_bytes` and `charset_object_name` had **no** referent outside this
file. `decode_utf16_bytes` survives as an unrelated private function of the
same name in `native-builtins/src/xml_stax.rs`, which has its own definition and
its own callers — the two never saw each other. No import is orphaned:
`ArrayElementType` keeps 30 uses in the file, `ObjectRef` 37, `NativeContext`
45, `obj_arg` 67, `NativeMethodRegistry` 53.

No test anywhere exercised the deleted bodies. `serialization.rs`'s own test
module (≈45 calls to `register_serialization_natives`) never once mentions
`ByteArrayOutputStream`; the `vm` crate's BAOS tests build a whole `Vm` /
`SharedVm`, so they go through `vm_init` and were already measuring native-io.

### 2.5 It is pinned, with the vacuity guard

`baos_registrar_is_empty_and_stays_empty` in `serialization.rs` asserts both
halves, on the model `G41-1` §3.4 sets:

* `register_byte_array_output_stream` must leave `registry.len()` unchanged and
  must bind none of `close`/`flush`/`toByteArray`/`<init>()V`/`write([BII)V` —
  the fix held;
* `register_serialization_natives` must still register **something** — without
  this the test would pass just as happily on the day the aggregate breaks,
  which is the "confident, vacuous zero" `F34-1` §2.1 recorded twice.

It is a `#[cfg(test)]` test inside a module gated
`any(experimental-serialization, synthetic-jdk)`, so it runs only under those
features. That is a real limit and it is stated rather than hidden: in a default
`cargo test -p cratonvm-native-builtins` this guard does not run at all.

---

## 3. `G41-1` N4 adjudicated: `java.time.Instant` — the fall-through is CORRECT

### 3.1 The standing counter-example, reproduced — MEASURED-ON-BINARY

`G41-1` §6 predicted from `native-api/src/registry.rs` that a shipping twin
tagged `SyntheticStub` is refused outright under `--jdk-only`, so **neither**
copy is registered. Reproduced exactly on `9ae371468`:

| | compatible | `--jdk-only` |
|---|---|---|
| `java/time/Instant` rows | **16** | **0** |
| `kind` | `synthetic-stub` ×16 | — |
| `owns_slot` | `true` ×16 | — |
| `registered_by` | `native-builtins/src/lib.rs:41698`–`:41783` | — |
| `--jdk-only-report` | — | **16 `synthetic-native-registered` violations**, same 16 sites |

(`G41-1` quoted `lib.rs:41174`/`:41202` from `9964ca733`; the same registrar sits
at `:41694`–`:41783` in this tree. The pass is
`register_synthetic_instant_stub_natives`, called from
`reflect_annotations.rs:758` inside `register_essential_natives_with_shims` —
i.e. on the SHIPPING path, under a name that says synthetic.)

The drifting synthetic-only half is `util_time::register_time_natives`, which
tags itself `NativeKind::Bridge`.

### 3.2 Is falling through to bytecode correct? — MEASURED-ON-BINARY: YES

Nobody had asked. `G41-1` N4 said so in as many words. Asked now, with a
58-assertion probe covering every part of the surface a two-slot synthetic tends
to get wrong:

* the three factories including `ofEpochSecond(s, ns)` nano **carry** and
  negative-nano **borrow**, and `ofEpochMilli(-1)`;
* `getEpochSecond`/`getNano`/`toEpochMilli` on a pre-epoch instant;
* `plusSeconds`/`minusSeconds`/`plusMillis`/`plusNanos` including both carry
  directions and negatives;
* `isBefore`/`isAfter`/`equals(null)`/`equals(String)`/`hashCode` stability;
* nine `toString` shapes — `EPOCH`, whole-second, milli, micro, nano,
  pre-epoch with and without a fraction, `0001-01-01T00:00:00Z`, and
  `9999-12-31T23:59:59Z`;
* `now()` invariants, `compareTo`, `isSupported`, `truncatedTo`, `atZone`,
  `Instant.parse` and a `parse(toString())` round-trip, `EPOCH`/`MIN`/`MAX`
  (`-1000000000-01-01T00:00:00Z` / `+1000000000-12-31T23:59:59.999999999Z`);
* both `ArithmeticException` overflow paths.

```
HotSpot 25.0.3+9-LTS   58 checks, exit 0
CratonVM compatible    58 checks, exit 0, stdout IDENTICAL
CratonVM --jdk-only    58 checks, exit 0, stdout IDENTICAL
```

Byte for byte, all three. **So the answer is yes**: for a real JDK class like
`java.time.Instant`, falling through to the JDK's own bytecode is the right
answer, and the sixteen `--jdk-only-report` rows are not blockers. They read
like blockers — `synthetic-native-registered` is the tag whose remedy line says
"go implement or re-tag it" — and the correct remedy here is neither: it is to
stop registering.

Note the compatible-mode arm passes for a **different reason**, and it matters.
There the stub owns all 16 slots, but `java/time/Instant` is on
`real_protected_stub_class_common`'s yield list in
`vm/src/runtime/interpreter/native_override.rs`, so
`synthetic_stub_kind_should_yield_to_real_bytecode` sends a loaded real
`Instant` to its own bytecode anyway. Two different mechanisms, one of them
per-class and hand-maintained, converging on the same correct outcome. That
convergence is undocumented anywhere the next reader would look, which is
nomination **N3**.

### 3.3 Why the sixteen were NOT deleted

`util_time::register_time_natives` is this lane's file and deleting its
`Instant` arms would have moved the drift total by another 16. It was not done,
and the difference from §2 is the whole argument:

* the `TreeMap` (`G41-1` §3) and `ByteArrayOutputStream` (§2) copies were proven
  **never to run in any build**, because a shipping registrar re-registered the
  same triples afterwards;
* these ones **do** run. In a `--features synthetic-jdk` build they are
  `Bridge`s registered *after* the essentials' `SyntheticStub`, so they win —
  and on a synthetic image there is no real `java.time.Instant` bytecode behind
  them to fall through to.

The two copies do share a layout (slot 0 epoch-seconds `Long`, slot 1 nano
`Int`: `INST_FIELD_EPOCH_SEC`/`INST_FIELD_NANO` against
`synthetic_instant_parts`), and they register the same 16 triples, so the stub
copy *probably* could serve synthetic mode alone. "Probably" is not the standard
this directory holds itself to, no synthetic build can be made on this machine,
and the disposition `util_time.rs` actually wants is T2.5.15's — delete the
module — not a 16-triple retirement. The adjudication is recorded in the file
itself, at the `// --- Instant ---` block, so the next lane finds it where the
code is rather than only here.

### 3.4 Does `util_time.rs` have any `--jdk-only` reach at all? — MEASURED: NO

The file's own header claimed this on 2026-08-11 from a **single-mode** census
of 11,665 registrations. Re-checked here in **both** modes:

| | compatible | `--jdk-only` |
|---|---|---|
| registry rows naming `native-builtins/src/util_time.rs` | **0** of 12,039 | **0** of 10,691 |
| registry rows naming `native-builtins/src/serialization.rs` | **0** of 12,039 | **0** of 10,691 |

`pub mod util_time;` carries `#[cfg(feature = "synthetic-jdk")]`, so the module
is not compiled into the shipping build at all. **An edit to either of this
lane's two files cannot change any shipping behaviour in any mode** — which is
the honest frame for §2 as well: the `ByteArrayOutputStream` deletion removes a
latent hazard and 12 census rows, not a live defect. The file header now records
the two-mode measurement.

---

## 4. The new drift total — PREDICTED, and how it was derived

`registrar_drift.rs` is not this lane's file and was not touched. Its
`DRIFT_TRIPLES` table was re-parsed instead, and the parse reproduces all three
of its constants exactly — 111 passes, 1,380 `(pass, triple)` pairs, 1,244
distinct triples — which is the check that makes the arithmetic below worth
quoting rather than guessing.

All twelve `java/io/ByteArrayOutputStream` rows are registered by
`register_byte_array_output_stream` and by **no other** synthetic-only pass, so
removing that pass removes twelve pairs and twelve distinct triples:

```
BASELINE_TOTAL_PAIRS   1_380 -> 1_368
BASELINE_TOTAL_DRIFT   1_244 -> 1_232
passes in DRIFT_TRIPLES  111 -> 110
```

**This lane's change reddens three of the gate's six tests until the table is
re-taken**, and that is stated up front rather than discovered:

* `the_drift_baseline_has_no_stale_rows` — 12 pairs no longer drift;
* `the_known_live_twins_still_drift` — `MUST_DRIFT` holds
  `java/io/ByteArrayOutputStream.close()V`, whose row already carries this
  lane's own MEASURED evidence for why it was safe to remove;
* `no_new_mode_drift` is unaffected in the adding direction.

The repair is `retake()`, which prints the replacement table on failure
(`G41-1` §7). Nomination **N2** gives the exact edit so it need not be
re-derived.

Two things that do **not** move, checked deliberately:

* `RESOLVER_WITNESSES` keeps `java/io/ByteArrayOutputStream.toByteArray()[B`.
  Its stated reason — "a `let cls = \"...\"` binding read out of the enclosing
  fn" — now witnesses `native-io`'s `let baos = "…";` instead of
  `serialization.rs`'s `let cls = "…";`. Same resolution path, same assertion
  (presence in the census, drifting or not), and the census must already resolve
  the native-io site or these twelve could never have been classified as
  drifting in the first place.
* `SYNTHETIC_ONLY_CLOSURE` in `registrar_reachability.rs` keeps the pass name:
  the function still exists and is still reachable only from
  `register_synthetic_overrides`.

---

## 5. Regression vectors — MEASURED-ON-BINARY

All nine, CratonVM against a HotSpot 25.0.3+9-LTS oracle, stdout diffed byte for
byte with line endings normalised, in **both** modes. None of the nine takes a
`class_cv_args` hook, so all ran as plain `-cp build <Class>`.

| vector | compatible | vs HotSpot | `--jdk-only` | vs HotSpot | result |
|---|---|---|---|---|---|
| `RSerial` | exit 0 | identical | exit 0 | identical | `PASS RSerial (21 checks)` |
| `RJdkNio` | exit 0 | identical | exit 0 | identical | `PASS RJdkNio (101 checks)` |
| `RFileTimes` | exit 0 | identical | exit 0 | identical | `PASS RFileTimes (68 checks)` |
| `RDataInputFastPull` | exit 0 | identical | exit 0 | identical | `PASS RDataInputFastPull (22 checks)` |
| `RJdkHello` | exit 0 | identical | exit 0 | identical | `PASS RJdkHello (41 checks)` |
| `RSimpleTimeZoneRaw` | exit 0 | identical | exit 0 | identical | `PASS RSimpleTimeZoneRaw (393 checks)` |
| `RSimpleDateFormatZone` | exit 0 | identical | exit 0 | identical | `PASS RSimpleDateFormatZone (115 checks)` |
| `RJdkFormatLocale` | exit 0 | identical | exit 0 | identical | `PASS RJdkFormatLocale (20 checks)` |
| `RCollections` | exit 0 | identical | exit 0 | identical | `PASS RCollections (53 checks)` |

**What this does and does not prove**, on `G41-1` §8's model. The binary predates
this lane's source change and cannot contain it. What these runs measure is the
*shipping* bodies — `native_baos_*` for the `ByteArrayOutputStream` family, real
JDK bytecode for `Instant` — which are exactly the bodies the change makes the
only copy in every mode. `RSerial` is the load-bearing one: the dump taken during
its run is where the non-zero `close`/`flush`/`write` invocation counts in §2.2
come from, so it is a direct check that the surviving implementation is the one
being exercised. The deletion's effect on a `--features synthetic-jdk` build is
**PREDICTED**: nobody in this lane could build one.

---

## 6. NOMINATIONS

**N1 — delete the two calls to `register_byte_array_output_stream`.**
`native-builtins/src/lib.rs:24935` (the direct call, whose 30-line comment about
"real-JCA DER output" was already corrected in place by wave 4 and is now
entirely about a function that registers nothing) and
`native-builtins/src/serialization.rs`'s
`register_serialization_natives`, whose call this lane deliberately left so the
two disappear together. The function is empty and documented as such; the calls
are harmless but the empty function is dead weight and an invitation to refill
it. `baos_registrar_is_empty_and_stays_empty` holds either way. `lib.rs` is not
this lane's file, which is the only reason it was not done here.

**N2 — re-take `registrar_drift.rs`, exactly.** Three edits, all mechanical:

1. delete the whole `("register_byte_array_output_stream", &[ … 12 rows … ]),`
   entry from `DRIFT_TRIPLES`;
2. `BASELINE_TOTAL_PAIRS: 1_380 -> 1_368`,
   `BASELINE_TOTAL_DRIFT: 1_244 -> 1_232`;
3. move the `java/io/ByteArrayOutputStream.close()V` row out of `MUST_DRIFT`
   and into `FIXED_NOT_DRIFTING` — with all twelve triples, not just `close`,
   so the vacuity half of `the_fixed_twins_stay_fixed` observes the whole
   family. All twelve remain in the census via `native-io`'s
   `register_io_natives`, which is what that half requires.

The positive control does **not** need re-pointing: `MUST_DRIFT`'s other three
rows (`java/time/Instant.getEpochSecond()J`,
`java/util/concurrent/atomic/AtomicBoolean.get()Z`,
`org/slf4j/Logger.debug(Ljava/lang/String;)V`) are untouched by this lane, and
§3.3 explains why the `Instant` row in particular must stay.

**N3 — `native-builtins/src/lib.rs`: rename the pass, and record why the
sixteen `Instant` triples are allowed to fall through.** Two parts, and the
second is the one that has never been written down:

* (a) `register_synthetic_instant_stub_natives` is on the SHIPPING path
  (`reflect_annotations.rs:758` → `register_essential_natives_with_shims`) under
  a name that says otherwise. `G3-1` N4 asked for the rename; it is still not
  done. `register_instant_bootstrap_fallback_natives` would say what it is —
  a fallback for a bootstrap that had to synthesize `Instant` before the real
  class was available.
* (b) The comment at the call site says "a loaded real-JDK `Instant` remains on
  its own bytecode path", which is TRUE but not for the reason a reader would
  assume, and by two different mechanisms in the two modes: in compatible mode
  by `real_protected_stub_class_common`'s per-class yield list in
  `vm/src/runtime/interpreter/native_override.rs`, and under `--jdk-only` by
  `allowed_in` refusing the whole `SyntheticStub` kind before it is ever
  registered. Both are now MEASURED (§3.1) and the outcome is MEASURED CORRECT
  (§3.2). Neither file says so. A one-line cross-reference in each direction
  would stop the next lane "fixing" this by promoting the stub to a `Bridge` —
  which would be strictly worse, because a `Bridge` **preempts** real JDK
  bytecode (`G34-1`) and would put a two-slot synthetic in front of a correct
  implementation.

**N4 — `native-io/src/lib.rs`: `new ByteArrayOutputStream(negative)` must
throw.** Unchanged from `G3-1` N3 and `G41-1` N3, and now the *only* remaining
place it can be fixed, since the `serialization.rs` copy that clamped with
`.max(1)` is gone. `native_baos_init_capacity` falls back to 32; HotSpot throws
`IllegalArgumentException: Negative initial size: -1`. Not drift — a defect the
surviving copy has on its own. Verified against JDK 25 `src.zip`.

**N5 — `registrar_reachability.rs` line 145 contradicts `registrar_drift.rs`,
and the dump says drift is right.** Reachability's verdict for
`register_byte_array_output_stream` reads

> `SHIPPING TWIN: no class exclusive to it; 0/12 triples also registered by a shipping pass`

while `DRIFT_TRIPLES` lists all 12 as drifting against a shipping pass, and the
registry dump shows `native-io/src/lib.rs` owning every one of them in both
modes. **Drift is right; reachability's `0/12` is wrong.** Two consequences:

* the verdict must become the documented TOMBSTONE string —
  `"TOMBSTONE: registers nothing (deliberately empty or dynamic-only); no capability rides on it"`
  — because after this lane the pass registers nothing at all;
* the `N/M triples` column of that table should be re-derived generally. If its
  census counts only `native-builtins`-side shipping registrars, then every
  `SHIPPING TWIN` row whose real twin lives in `native-io`,
  `native-collections`, `native-builtins-crypto` or `native-builtins-security`
  understates its drift by the same mechanism, and the whole column is
  advisory rather than measured. That is a lane, and it is the first
  cross-check anyone has run between these two gates.

**N6 — the `--jdk-only-report` needs a third disposition.** All 16 `Instant`
rows are `synthetic-native-registered`, a tag whose remedy is "implement or
re-tag". §3.2 measures that the correct remedy is a third thing: **delete the
registration, the JDK already does this correctly**. There are 1,341
`synthetic-native-registered` violations in a single `RJdkHello`-class run; if
even a modest fraction are `Instant`-shaped, the report's headline number is
overstating the work by that fraction. `G41-1` §6 already measured 122 of 1,244
drift rows as "`--jdk-only` runs neither copy". A pass over those 122 asking
`Instant`'s question of each — *is the JDK's own answer already right?* — is the
cheapest large reduction available, and §3.2's probe shape is the instrument.

**N7 — carried forward, untouched:** `G41-1` N5 (the 935 closure-vs-closure
rows), N6 (the 140 dump-absent triples), N7 (`registrar_reachability.rs` should
adopt the brace-balance self-check) and N8 (`NativeKind` is not statically
recoverable).

---

## 7. Verified vs assumed

**Verified.**

* Every row in §2.2, §3.1 and §3.4 — read out of two `--dump-native-registry`
  JSON files and one `--jdk-only-report` JSON file produced by `9ae371468`.
* §3.2's 58-assertion differential, run on three VMs, diffed with `\r` stripped.
* §5's nine vectors, both modes, against HotSpot 25.0.3+9-LTS.
* That `register_byte_array_output_stream` registered exactly 12 triples on
  exactly one class with no loop and no `format!` — parsed from the source
  before deletion, and the same 12 appear in the dump as native-io rows.
* That the five deleted helpers had no referent elsewhere — `grep` across all
  seven crates plus `vm-cli`, excluding `target/` and `scratch*/` — and that no
  import is orphaned (use counts in §2.4).
* That `serialization.rs`'s test module never touched `ByteArrayOutputStream`,
  so nothing existing breaks.
* That both edited files are LF-only (`tr -cd '\r' | wc -c` = 0) and carry the
  **same** pre-existing `rustfmt --edition 2021 --check` hunks as `HEAD` — 10 in
  `serialization.rs`, 6 in `util_time.rs` — with byte-identical hunk bodies. No
  new hunks.
* §4's re-parse reproducing `DRIFT_TRIPLES`' three constants exactly.

**Assumed / not verified.**

* **That `serialization.rs` still compiles**, and that its two new assertions
  type-check. No `cargo`. The new test uses only `NativeMethodRegistry::new()`,
  `len()` and `find()`, all of which appear in the tests immediately below it,
  and needs no VM trait bounds — deliberately, because `GarbageCollector` is
  `Sync` and a `RefCell` double does not satisfy it.
* **That `registrar_drift.rs` compiles.** Fourth lane running.
* **That the new totals are 1,368 / 1,232.** They are arithmetic over the gate's
  own table, not output from the gate's own scanner. §4 says how to repair if
  they differ.
* **That deleting the twelve arms changes nothing in a `--features
  synthetic-jdk` build.** It follows from `vm_init.rs`'s statement order and
  from `register()` being last-write-wins on the callback, both read in the
  source; no synthetic build was made. This is the single largest caveat on §2.
* That §5's nine vectors would still pass with the change compiled in. They
  exercise the surviving bodies, which the change does not touch.

---

## 8. What this lane did NOT do

1. **It did not build or run `cargo`.** Fourth lane running.
2. **It did not close the `Instant` family**, only adjudicate it (§3.3). 16
   triples still drift.
3. **It did not touch `native-io/src/lib.rs`, `native-builtins/src/lib.rs`,
   `native-builtins/tests/registrar_drift.rs`,
   `native-builtins/tests/registrar_reachability.rs`,
   `phases_late/collections.rs`, `INDEX.md` or `README.md`** — nominations only.
4. **It did not chase N5's contradiction between the two gates**, only measure
   the one instance that stood in its way.
5. **It did not check registration ORDER within a mode from a dump.** Unchanged
   from `G41-1` §10.6: `overwrote` was `null` on every row this lane read, which
   is consistent with the synthetic copies simply not being compiled in.
