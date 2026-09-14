# F3 — `java.util.Random`'s null contract lives in a file this lane cannot edit, and the second `java/util/logging/Logger` convention in `lib.rs`

**Date:** 2026-08-13 **Lane:** F3
**Closes:** NOM E41-1 (the `lib.rs` half), and diagnoses
`RJdkIntrinsics2 --only=random` check **41 of 60**.

**This lane did not build or run CratonVM, and did not run `cargo`.** Every
CratonVM "after" below is **PREDICTED**. Evidence is (a) source read in this
working tree, (b) the JDK 25 source checkout at `C:\craton\jdk25src`, and
(c) **HotSpot 25.0.3+9 runs I made myself** — the oracle table in §1.2 and the
check-index table in §1.1 are MEASURED, not remembered.

## 0. Files edited

| file | why it is mine |
|---|---|
| `native-builtins/src/lib.rs` | assigned, exclusively owned |
| this document | assigned (new `.md` under `docs/known-issues/jdk-only/`) |

**Task 1 produced no edit**, because the code it names is not in my file. See
§1.3 and NOM F3-1.

---

## 1. TASK 1 — `java.util.Random`'s null contract

### 1.1 Where the run stops, and what is behind it

`check()` **throws** on the first failure (`RJdkIntrinsics2.java:100-105`), so
the reported failure is the *first* one and everything after it is unrun. I
instrumented a copy of the vector to print each check's index and ran it on
HotSpot 25.0.3+9:

| checks | status |
|---|---|
| 1 – 40 | pass on CratonVM (they ran, and the run reached 41) |
| **41** | `nextBytes(null) must throw NullPointerException, got none` — **the failure** |
| 42 – 60 | **never executed on CratonVM.** 19 checks, all green on HotSpot |

`random` is 60 checks; the family PASSES 60/60 on HotSpot with no flags.

### 1.2 The oracle table — the contracts are not uniform

Measured on OpenJDK 25.0.3+9 (`scratchpad/f3/Oracle.java`), not recalled:

| call | HotSpot 25 |
|---|---|
| `nextBytes(null)` | **NPE** `Cannot read the array length because "bytes" is null` |
| `nextBytes(new byte[0])` | no throw |
| `nextInt(0)` / `nextInt(-5)` / `nextInt(MIN_VALUE)` | IAE `bound must be positive` |
| `nextInt(10,10)` / `nextInt(10,5)` | IAE `bound must be greater than origin` |
| `nextInt(MIN_VALUE, MAX_VALUE)` | no throw |
| `nextLong(0)` / `nextLong(-1)` | IAE `bound must be positive` |
| `nextLong(10,10)` | IAE `bound must be greater than origin` |
| `nextDouble(0.0 / NaN / +Inf / -1.0)` | IAE `bound must be finite and positive` |
| `nextDouble(1.0,1.0)` / `nextDouble(1.0,NaN)` | IAE `bound must be greater than origin` |
| `nextFloat(0f)` / `nextFloat(NaN)` | IAE `bound must be finite and positive` |
| `setSeed(Long.MIN_VALUE)` | **no throw — `setSeed` accepts anything** |
| `ints(-1)` / `longs(-1)` / `doubles(-1)` | IAE `size must be non-negative`, **at call time, before `toArray()`** |
| `ints(5,10,10)` / `ints(10,10)` / `doubles(5,NaN,1.0)` | IAE `bound must be greater than origin` |
| `Random.from(null)` | NPE |
| `SecureRandom.nextBytes(null)` | NPE |
| `SecureRandom.setSeed((byte[])null)` | NPE |
| `new SecureRandom((byte[])null)` | NPE |
| `SecureRandom.generateSeed(-1)` | IAE **`numBytes cannot be negative`** |
| `SecureRandom.getInstance(null)` | **NPE** `null algorithm name` |
| `SecureRandom.getInstance("")` | **NoSuchAlgorithmException** `" SecureRandom not available"` (leading space is real) |

Sources: `Random.java:458` (`@throws NullPointerException if the byte array is
null`) and `:462-467` (the body opens `bytes.length`), `RandomSupport.java:65-69`
(`BAD_SIZE` / `BAD_BOUND` / `BAD_FLOATING_BOUND` / `BAD_RANGE`),
`SecureRandom.java:391,878`.

### 1.3 Where the surface actually is — **not `lib.rs`**

`native-builtins/src/lib.rs` registers **no** `java/util/Random` triple. What
it has are two *calls* — `register_essential_natives_with_shims`
(`lib.rs:19289`, promoted there so it also covers real-JDK mode) and the tail of
`register_security_natives` (`lib.rs:36923`) — both of the form

```rust
crate::securerandom::register_random_and_securerandom_natives(registry);
```

The eleven `java/util/Random` triples, and every `SecureRandom` one, are
registered and implemented in **`native-builtins/src/securerandom.rs`**
(`:1544-1555`). Two other registrars exist and both LOSE:

* `native-collections/src/lib.rs:31323 register_random_natives` — a synthetic
  2-field-layout copy with **no `nextBytes` at all**;
* `vm/src/vm/vm_init.rs:2820-2833` re-registers `securerandom`'s **after**
  `register_collections_natives` precisely because the collections copy reads
  the LCG seed from instance field 0, which in real-JDK mode is the
  `AtomicLong seed` reference — a seeded `Random` returned all zeroes.

The measured behaviour confirms which one answers: `nextBytes(new byte[7])`
produces the byte-exact JDK sequence (check 37 passes) **and** `nextBytes(null)`
throws nothing — that is `securerandom::native_random_next_bytes` exactly, whose
second `match` arm is `_ => return Ok(None)` and therefore swallows
`Value::Object(None)`.

So Task 1 is **NOM F3-1**, with literal old/new text. I did not add a
compensating registration in `lib.rs`: a `nextBytes` re-registered after the
`securerandom` call would be a *second* implementation of a triple that already
has three registrars, which is the exact shape §2 of this document is deleting.

### 1.4 The audit, member by member

`java/util/Random`, all eleven registered triples:

| triple | contract | `securerandom.rs` today | verdict |
|---|---|---|---|
| `<init>()V` | — | entropy seed | OK |
| `<init>(J)V` | any `long` | scramble | OK |
| `setSeed(J)V` | **accepts anything** | scramble + clear gaussian cache | OK |
| `nextInt()I` | — | LCG `next(32)` | OK |
| `nextInt(I)I` | `bound<=0` → IAE `bound must be positive` | present, **exact wording** | OK |
| `nextLong()J` `nextDouble()D` `nextFloat()F` `nextBoolean()Z` | — | LCG | OK |
| `nextBytes([B)V` | **null → NPE** | `_ => return Ok(None)` | **DEFECT** |
| `nextGaussian()D` | — | polar + cached partner | OK |

The `nextInt(I)` guard is the proof that this family was audited once for
*range* and never for *null*: it is the only rejection in the file that matches
the JDK's own message string, and the only member with an argument that can be
out of range. The one member with an argument that can be **null** has no check.

Same shape in the same file, found by diffing the family rather than the row
([no-op w/ excuse], [1 of 10 callsites]):

| triple | contract | today | verdict |
|---|---|---|---|
| `SecureRandom.nextBytes([B)V` | null → NPE | `_ => return Ok(None)` | **DEFECT (twin)** |
| `SecureRandom.setSeed([B)V` | null → NPE | `_ => return Ok(None)` | **DEFECT (twin)** |
| `SecureRandom.<init>([B)V` | null → NPE | ignores the arg | **DEFECT (twin)** |
| `SecureRandom.generateSeed(I)[B` | `n<0` → IAE `numBytes cannot be negative` | IAE **`numBytes must be non-negative`** | wording divergence |
| `SecureRandom.getInstance(String)` | null → **NPE**; `""` → **NoSuchAlgorithmException** | one IAE for both | **wrong exception TYPE, twice** |
| `SecureRandom.nextInt(I)I` | `bound<=0` → IAE | present, exact wording | OK |

The `getInstance` row is the interesting one: the existing code already builds
the right `NoSuchAlgorithmException` message a few lines further down
(`format!("{algo} SecureRandom not available")`), and with `algo == ""` that
formats to `" SecureRandom not available"` — **character-for-character HotSpot's
answer for the empty name**. The `if algo.is_empty()` early return is the only
thing standing between the file and the correct behaviour, and it was written to
serve the *null* case.

### 1.5 Which `--only=random` checks flip — PREDICTED

With NOM F3-1 applied:

* **check 41 flips red → green.** `nextBytes(null)` raises
  `RuntimeError::NullPointerException`, which
  `types/src/error.rs:1478` maps to `java/lang/NullPointerException`, which is
  what `nameOf(t)` compares. This is the only check in the family that the fix
  touches directly.
* **checks 42–60 become REACHABLE for the first time.** They have never run on
  CratonVM in any form, so their status is unknown, not "expected green". They
  are also unaffected by the fix itself — every one of them goes through *real
  JDK bytecode*, because none of their triples is registered:

| checks | route | risk assessment |
|---|---|---|
| 42–46 `nextInt(10,20)` | `RandomGenerator.nextInt(int,int)` default → `RandomSupport.checkRange` → `boundedNextInt`, which calls **only `rng.nextInt()`** (`RandomSupport.java:496,508,515`) — intercepted, LCG, deterministic | LOW |
| 47–51 `nextLong(J)`, `nextLong(JJ)`, `nextDouble(D)`, `nextDouble(DD)`, `nextFloat(F)` | same shape, over `nextLong()`/`nextDouble()`/`nextFloat()` | LOW |
| 52 `nextExponential()` | `RandomSupport.computeNextExponential` → `DoubleZigguratTables` static tables + `rng.nextLong()`. The FAST path is a table lookup times a long, no `Math` call; the slow path calls `Math.exp`/`Math.log` | MEDIUM — depends on `DoubleZigguratTables.<clinit>` loading, and on fdlibm if the slow path is taken. Compared **to the bit** |
| 53–56 `ints/longs/doubles` | `AbstractSpliteratorGenerator` → spliterator → `StreamSupport` → `toArray()` | MEDIUM — the most machinery of any row here |
| 57 `new Random()` × 2 | `native_random_init_noseed`, OS entropy, side table keyed by identity hash | LOW |
| 58 `nextInt(MAX_VALUE)` | intercepted. Hand-checked: `next(31)` for seed 42 is `(unsigned(-1170105035)) >>> 1 = 1562431130`, the expected value; the rejection loop's `r.wrapping_sub(candidate).wrapping_add(m) >= 0` correctly rejects `r == bound` | LOW |
| 59 `nextInt(10,10)` | `RandomGenerator` default → `checkRange` → IAE | LOW |
| 60 `nextLong(0)` | `RandomGenerator` default → `checkBound` → IAE | LOW |

**One hazard worth naming for whoever runs this next.**
`phases_late.rs:5010 register_p64_random_generator` registers
`nextInt()I`, `nextInt(I)I`, `nextLong()J`, `nextDouble()D`, `nextFloat()F` and
`nextBoolean()Z` **on the interface `java/util/random/RandomGenerator`**, backed
by `p64_simple_random()` — a receiver-ignoring, non-LCG generator. Checks 1–40
passing proves the class-level `java/util/Random` registration wins for a
`Random` receiver today. But every one of checks 42–60 enters
`RandomGenerator`'s own default methods, and if that interface's natives are
ever consulted for a `Random` receiver, the LCG stream is replaced wholesale and
**checks 42–56 fail as a block with plausible-looking random numbers**. That
would be a divergence with no null and no exception in it, so it would not look
like this defect at all. Recorded, not investigated.

---

## 2. TASK 2 — the second `java/util/logging/Logger` convention in `lib.rs`

### 2.1 What was there

`classloading/src/class_manager.rs:14191` declares
`java/util/logging/Logger` with real-JDK field names and order — `config` 0,
`manager` 1, **`name` 2**, … `parent` 8, plus `vm_internal_field(12)` for the
level. `logmanager.rs:164-170` is the one convention that matches it
(`LOGGER_NUM_FIELDS` 13, `LOGGER_FIELD_NAME` 2, `_PARENT` 8, `_LEVEL` 12), and
eight sites in `lib.rs` already imported it.

`lib.rs:36353-36354` declared a **second, local** one:

```rust
const LOGGER_FIELD_NAME: usize = 0;
const LOGGER_FIELD_LEVEL: usize = 1;
```

— i.e. the name `String` written into `config: Logger$ConfigurationData` and the
level into `manager: LogManager`. Four bodies read it.

**Width was never the hazard.** `Logger` *is* declared, so
`try_alloc_concurrent_synthetic`'s closing `num_fields.max(real)` clamped every
3-slot ask up to 13 and no `Heap::get_field` assert could ever fire. What was
broken was the field's *identity*, and the clamp does not protect that. Judging
this family by width would have found nothing.

### 2.2 The four bodies, and what each did

| body | did | now |
|---|---|---|
| `native_logger_get` (`Logger.getLogger(String)`) | minted a **3**-slot Logger, name → slot 0, a freshly-allocated INFO `Level` → slot 1 | delegates to `logmanager::get_or_create_logger` |
| `native_logger_get_global` (`Logger.getGlobal()`) | same, name `"global"` | `get_or_create_logger(ctx, "global")` |
| `native_logger_get_level` | `get_field(this, 1)` → returns `manager: LogManager` **typed as a `Level`** | `get_field(this, logmanager::LOGGER_FIELD_LEVEL)` behind a width guard |
| `native_logger_log_if` | read slot 1 as a `Level`, then asked it for `LEVEL_FIELD_VALUE` | reads slot 12, decodes both the `Level`-reference and raw-`Int` shapes |

`native_logger_get_name` already delegated to `logmanager::jul_logger_name_object`
and is unchanged; `native_logger_log` / `native_logger_log_level` index no Logger
slot and are unchanged.

Two of the rewrites are delegations rather than re-slotted copies, because a
correct helper already existed and was `pub(crate)`:

* `logmanager::get_or_create_logger` (`:852`) is the tree's one JUL Logger
  producer. It allocates at `LOGGER_NUM_FIELDS`, writes `LOGGER_FIELD_NAME`,
  **caches by name**, links `LOGGER_FIELD_PARENT` to the nearest existing dotted
  ancestor, re-parents descendants, and pins across every allocating step. The
  deleted body did none of that. Delegating also makes
  `Logger.getGlobal() == Logger.getLogger("global")` hold, which is the JDK
  contract (`Logger.GLOBAL_LOGGER_NAME` is `"global"`) and which minting a fresh
  Logger per call could not satisfy.
* `native_logger_get_level` / `native_logger_log_if` grew the
  `object_num_fields(this) <= LOGGER_FIELD_LEVEL` guard that
  `logmanager::native_jul_logger_is_loggable` (`:5293`) already uses, for the
  reason that reader has it: `Heap::get_field` asserts `index < num_slots`, and
  slot 12 is past the end of any Logger sized from a stub declaration.

`LEVEL_FIELD_NAME` / `LEVEL_FIELD_VALUE` are **kept**, and they are now
*verified* rather than assumed: `javap -p java.util.logging.Level` on JDK 25
gives instance fields `name` 0, `value` 1, `resourceBundleName` 2,
`localizedLevelName` 3, `cachedLocale` 4. So 0/1 is exact for the real class and
a prefix of the VM's 2-slot synthetic `Level`, and
`register_essential_natives_with_shims` registers `native_level_init` in every
mode.

### 2.3 The stale comment at `:17599`

The comment sitting between a real-JDK `setLevel` arm and three lines that
already use the `logmanager` constants read:

> Our `allocate_logger`-created synthetic loggers (slot0=name, slot1=level,
> slot2=parent) …

`allocate_logger` (`logmanager.rs:668`) writes `LOGGER_FIELD_NAME` **2**,
`LOGGER_FIELD_PARENT` **8**, `LOGGER_FIELD_LEVEL` **12**. The comment described
neither the producer it named nor the reader it introduced — a retired 3-field
shim layout, i.e. the very convention deleted in §2.2. Replaced with the
declared layout and a note on what it used to claim.

### 2.4 Why the four bodies were corrected rather than deleted

NOM E41-1 asked for deletion. They cannot be deleted from `lib.rs` alone:
`logging_shims.rs:1799`, `:1805`, `:1855-1873` and `:1899` still name
`native_logger_get`, `native_logger_get_global`, `native_logger_log_if` and
`native_logger_get_level`, and `logging_shims.rs` is not mine. Deleting the
bodies would leave another lane's file unable to compile. So this lane removed
the **convention** — which is the part that can silently produce a wrong answer
— and left the removal of the (now correct, still shadowed) bodies to NOM F3-2.

I verified the shadowing myself rather than taking it from E41. In
`register_synthetic_overrides`:

```
24011  logging_shims::register_logging_natives     ← registers the four bodies
24012  logging_shims::register_slf4j_natives       ← registers a SUPERSET
24357  logmanager::register_logmanager_natives
```

`register_slf4j_natives` (`logging_shims.rs:2680`) registers, on
`java/util/logging/Logger`: `getLogger`, `getGlobal`, `info`, `warning`,
`severe`, `fine`, `finer`, `finest`, `config`, `log(Level,String)`,
`isLoggable`, `setLevel`, `getLevel`, `getName`, `addHandler`, `removeHandler` —
every triple `register_logging_natives` registers, one line later, and
registration is last-write-wins. So all four bodies are unreachable through the
registry **today**. "Currently shadowed" is a property of call ORDER inside one
function, not of the code; that is exactly why the map they carried had to go
now rather than with them.

### 2.5 PREDICTED after

No behaviour change is expected in any mode, because the four bodies are
shadowed in both. What changes is the failure that becomes impossible: a
reordering of `lib.rs:24011-24012`, or any new caller of these four functions,
can no longer mint a Logger whose name is in `config`, hand a `LogManager` to a
caller expecting a `Level`, or leave `logger.setLevel(SEVERE)` silently
unenforced. The one *observable* improvement if they ever do become reachable is
that `Logger.getGlobal()` and `Logger.getLogger("global")` become the same
object, as on HotSpot.

Syntax of the edited `lib.rs` was checked by parsing a copy with `rustfmt`
(it reaches submodule resolution, i.e. the file itself parses); the check was
calibrated by confirming rustfmt reports an injected `fn oops( {`. No `cargo`
was run. The diff is six hunks, all in `lib.rs`, LF-clean.

---

## 3. NOMINATIONS

### NOM F3-1 — `native-builtins/src/securerandom.rs` — the null contract across `Random` / `SecureRandom`

Ranked. **(a) is the one that flips a measured regression check**; (b)–(d) are
the same defect in the same file found by diffing the family; (e)–(f) are
exception-type and wording divergences found by the same audit.

#### (a) `java.util.Random.nextBytes(byte[])` — null must throw NPE

OLD:

```rust
pub(crate) fn native_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
```

NEW:

```rust
pub(crate) fn native_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `Random.nextBytes` is specified `@throws NullPointerException if the
        // byte array is null` (`Random.java:458`) and its body opens
        // `bytes.length`. A `Value::Object(None)` here IS that null, and this
        // arm used to swallow it: `new Random(42).nextBytes(null)` returned
        // normally, which `RJdkIntrinsics2 --only=random` check 41 reports as
        // "got none". Message measured on OpenJDK 25.0.3+9.
        Some(Value::Object(None)) => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some(
                        "Cannot read the array length because \"bytes\" is null".to_string(),
                    ),
                }
                .into(),
            );
        }
        // A missing or non-reference argument is an arity/marshalling bug, not
        // a Java null — keep the defensive return rather than reporting an NPE
        // the program did not cause.
        _ => return Ok(None),
    };
```

#### (b) `SecureRandom.nextBytes(byte[])` — the twin

OLD:

```rust
pub(crate) fn native_secure_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
```

NEW:

```rust
pub(crate) fn native_secure_random_next_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // `SecureRandom.nextBytes(null)` NPEs on HotSpot 25 (measured), same as
        // the `java.util.Random` parent — see `native_random_next_bytes`.
        Some(Value::Object(None)) => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException {
                    message: None,
                }
                .into(),
            );
        }
        _ => return Ok(None),
    };
```

#### (c) `SecureRandom.setSeed(byte[])` — the twin

OLD:

```rust
pub(crate) fn native_secure_random_set_seed_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(this) = secure_random_receiver(args) else {
        return Ok(None);
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
```

NEW:

```rust
pub(crate) fn native_secure_random_set_seed_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The null check precedes the receiver check: `setSeed(null)` NPEs on
    // HotSpot 25 (measured) regardless of algorithm, and the SHA1PRNG-only
    // early return below would otherwise swallow it for every other algorithm.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    let Some(this) = secure_random_receiver(args) else {
        return Ok(None);
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
```

#### (d) `new SecureRandom(byte[])` — the twin

`native_secure_random_init` serves BOTH `<init>()V` and `<init>([B)V`
(`securerandom.rs:1566`, `:1580`), so the check must be arity-aware.

OLD:

```rust
pub(crate) fn native_secure_random_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Per the JDK SecureRandom contract the no-arg ctor selects a default
    // provider; we always select "OS-CSPRNG", the strongest source available.
    secure_random_record_algorithm(ctx, args)?;
    Ok(None)
}
```

NEW:

```rust
pub(crate) fn native_secure_random_init(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Per the JDK SecureRandom contract the no-arg ctor selects a default
    // provider; we always select "OS-CSPRNG", the strongest source available.
    //
    // This body is registered for BOTH `<init>()V` and `<init>([B)V`, so the
    // null check is arity-gated: `new SecureRandom((byte[]) null)` NPEs on
    // HotSpot 25 (measured) — the seed reaches `engineSetSeed` before anything
    // can discard it. `<init>()V` has no second argument and is unaffected.
    if args.len() >= 2 && matches!(args.get(1), Some(Value::Object(None))) {
        return Err(
            cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into(),
        );
    }
    secure_random_record_algorithm(ctx, args)?;
    Ok(None)
}
```

#### (e) `SecureRandom.getInstance(String)` — null is an NPE, empty is a NoSuchAlgorithmException

The file already builds the correct empty-name message a few lines below:
`format!("{algo} SecureRandom not available")` with `algo == ""` is
`" SecureRandom not available"`, which is HotSpot's answer character for
character (measured). Deleting the `is_empty` early return is therefore both
halves of the fix.

OLD:

```rust
    let algo = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    if algo.is_empty() {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "null algorithm name".to_string(),
            }
            .into(),
        );
    }
```

NEW:

```rust
    // Two different answers, not one. `getInstance(null)` is
    // `Objects.requireNonNull(algorithm, "null algorithm name")`
    // (`SecureRandom.java:391`) — a NullPointerException. `getInstance("")` is
    // a NoSuchAlgorithmException whose message is `" SecureRandom not
    // available"`, which the `secure_random_algorithm_supported` branch below
    // already produces for the empty string. Both were collapsed into one
    // IllegalArgumentException, so the type was wrong for null and the type and
    // the wording were wrong for "". Measured on OpenJDK 25.0.3+9.
    let algo = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        Some(Value::Object(None)) | None => {
            return Err(
                cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("null algorithm name".to_string()),
                }
                .into(),
            );
        }
        _ => String::new(),
    };
```

#### (f) `SecureRandom.generateSeed(int)` — the message

OLD:

```rust
                message: "numBytes must be non-negative".to_string(),
```

NEW:

```rust
                // `SecureRandom.java:878` verbatim. "must be non-negative" is
                // `RandomSupport.BAD_SIZE`, which is a DIFFERENT method's
                // message (`ints`/`longs`/`doubles` stream size).
                message: "numBytes cannot be negative".to_string(),
```

(Anchor is unique in the file — one occurrence.)

**Not offered as a change, but worth a decision:** every `java.util.Random`
member that takes an origin/bound pair, plus all three primitive streams, is
UNREGISTERED and therefore served by real JDK bytecode. That is why they get the
right `IllegalArgumentException` for free today. If a future wave registers any
of them, it inherits the whole of §1.2's table at once — `BAD_BOUND`,
`BAD_RANGE`, `BAD_FLOATING_BOUND` and `BAD_SIZE` are four different messages and
`ints(-1)` throws at CALL time, not at `toArray()`.

### NOM F3-2 — `native-builtins/src/logging_shims.rs` + `native-builtins/src/lib.rs` — retire the shadowed `register_logging_natives` Logger half

The successor to NOM E41-1's deletion half, now that the convention is gone and
the bodies are correct. Both sides must land in ONE diff.

In `logging_shims.rs::register_logging_natives`, delete the fifteen
`java/util/logging/Logger` registrations at `:1795-1933` — `getLogger`,
`getGlobal`, `getName`, `addHandler`, `info`, `warning`, `severe`, `fine`,
`finer`, `finest`, `config`, `setLevel`, `getLevel`, `isLoggable`,
`log(Level,String)`. Every one is re-registered by `register_slf4j_natives` on
the very next line (`lib.rs:24012`), which is a strict superset, so no coverage
is lost. Keep the `java/util/logging/Level` half of the function.

Then delete from `lib.rs`, which by then has no callers: `native_logger_get`
(`:36409`), `native_logger_get_global` (`:36423`), `native_logger_get_name`
(`:36435`), `native_logger_get_level` (`:36457`), `native_logger_log_if`
(`:36490`), `native_logger_log_level` (`:36519`). **Check `native_logger_log`
(`:36470`) separately** — it has no reference anywhere in the tree today and may
already be orphaned.

Not offered as literal old/new text: the registration block is ~140 lines with
five inline closures, and `logging_shims.rs` is under concurrent edit by the
lane that last rewrote it.

### NOM F3-3 — `native-builtins/src/logmanager.rs:5287-5290` — a comment that names the retired slot

Noticed while establishing which reader is canonical. `native_jul_logger_is_loggable`'s
fallback comment says:

> a synthetic Logger keeps it in slot 1, holding EITHER a `Level` object or the
> raw int

The code three lines below it reads `LOGGER_FIELD_LEVEL`, which is **12**. "Slot
1" is `manager: LogManager` — the retired convention this lane deleted from
`lib.rs`. The code is right and the comment is wrong; it is the same false
sentence that made the `lib.rs` copy look intentional. Comment-only, in a file
that is not mine.

---

## 4. The lesson

**A family audited once for one kind of argument has not been audited.**
`securerandom.rs`'s `Random` surface carries the JDK's `bound must be positive`
message character for character — someone read the spec, for the *one* member
with a range. The *one* member with a nullable argument, in the same function
list, twenty lines away, swallows null and returns normally. The audit was
organised by the check it was performing, not by the members it had to cover, so
the member with a different contract was never in scope. §1.2's table is
deliberately the whole surface rather than the failing row, and it found four
more instances of the same swallow and two wrong exception types.

**And the second lesson is E41's, confirmed from the other side.** Judging the
`lib.rs` Logger convention by width would have cleared it: `Logger` is declared,
the clamp lifts every 3-slot ask to 13, no assert can fire. What was wrong was
which *named field* each number pointed at — a `String` in `config`, a `Level`
in `manager`, a `LogManager` returned to a caller expecting a `Level`. The
clamp protects the heap; nothing protects the field's identity, and no gate in
this tree measures it.
