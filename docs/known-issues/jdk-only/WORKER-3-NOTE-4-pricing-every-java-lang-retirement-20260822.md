# WORKER-3-NOTE-4 — pricing every `java.lang` retirement against one binary, and what it says about the three blocks

**2026-08-22.** All figures MEASURED on `e080feaa2` unless marked ARGUED.

`H22-3` is titled *a corpus arm cannot price a source retirement*, and `H25-2`
§3.1 gives the mechanism: the enforcement dial has one live call site inside
`resolve_step1_native`, so arming a class is at once **over-broad** (it
suppresses rows a deletion would keep) and **under-broad** (it misses the
warm-cache, force-native, reflective and JIT doors a deletion closes). Every
"do not retire" verdict in this lane's inheritance — including the one this
brief prints in bold — rests on an armed measurement.

So the first thing built here was the missing instrument.

## 1. The instrument — a registration gate, not a dispatch gate

`W3_RETIRE=<class>[,<class>…]` at `NativeMethodRegistry::register`
(`native-api/src/registry.rs`), the registry's single choke point. A named class
registers **nothing**, which is what deleting its registrar does and what the
dial cannot do — it closes every door at once rather than one.

Verified it does what it claims: `--dump-native-registry` with the three builder
classes named goes **10352 → 10166 rows, builder-class rows 186 → 0**.

One binary prices any candidate. Nineteen groups below cost 19 suite runs and
**one** build, against ~7 hours of rebuilds for the same coverage.

> Deliberately not a `CRATONVM_` name: that prefix is a declared flag surface
> with an inventory and a guard test, and an experiment must not add a row to it.

## 2. The pricing table — `CRATONVM_ARGS=--jdk-only`, 107 vectors

Control: **107 passed, 0 failed, census 1413.** (Not the brief's 1387: that was
105 vectors, two have since been added, and the census is a UNION over vectors.
Same tree, same binary for every row below.)

| group | rows | passed | census | Δcensus | verdict |
|---|---:|---:|---:|---:|---|
| `exceptions` (21 classes) | 31 | **107** | 1383 | **−30** | **CLEAN — retire** |
| `threadgroup` | 3 | **107** | 1410 | **−3** | **CLEAN — retire** |
| `enum` | 2 | **107** | 1412 | −1 | **CLEAN — retire** |
| `sb_only` | 45 | 107 | 1406 | −7 | clean but a TRAP — §4 |
| `sbuf_only` | 12 | 107 | 1411 | −2 | clean but a TRAP — §4 |
| `asb_only` | 0 | 107 | 1413 | 0 | inert alone — §4 |
| `boxes` | 4 | 106 | 1409 | −4 | 1 fail (`RJdkReflBox`) |
| `module` | 15 | 100 | 1328 | −85 | 7 fails |
| `stack` | 2 | 105 | 1414 | +1 | 2 fails |
| `ref` | 11 | 95 | 1291 | −122 | 12 fails |
| `sb_plus_asb` | 45 | 95 | 1286 | −127 | 12 fails — §4 |
| `builders_all` | 57 | 95 | 1281 | −132 | 12 fails — §4 |
| `invoke` | 51 | 82 | 1255 | −158 | **25 fails** |
| `classloader` | 9 | 73 | 1190 | −223 | 34 fails |
| `thread` | 26 | 67 | 1092 | −321 | 40 fails |
| `runtime` | 6 | 59 | 942 | −471 | 48 fails |
| `system` | 17 | 34 | 787 | −626 | 73 fails |
| `object` | 2 | 19 | 500 | −913 | 88 fails |
| `class` | 37 | 5 | 216 | −1197 | **102 fails** |

**The whole free lunch in the `java.lang` core block is 34 census rows** —
`exceptions` + `threadgroup` + `enum`. Everything else is load-bearing, and the
two biggest row-counts (`Class` 37, `Thread` 26) are the two most catastrophic.

`exceptions` being free is the row `H14-2` §5 predicted: it lists
`register_exception_extras_natives` at 22 rows with **21 of 22 registered on a
class that does not declare the method**. A registration that only ever shadowed
an inherited method costs nothing to remove. That prediction is now MEASURED.

## 3. `java/lang/invoke` — refusal, now at source level

51 rows, **25 vectors lost**. This lane previously refused the invoke block on an
ARMED measurement showing `NoSuchMethodError` for `BoundMethodHandle`,
`LambdaFormEditor` and `MethodType`. The source-level number is the one that
settles it: the block is not a tagging problem and not retirable, it is a
missing LambdaForm implementation. **Refusal stands, with better evidence.**

## 4. `StringBuilder` — the brief is right, and for a different reason than it says

### 4.1 H0's isolation reproduced at SOURCE level

H0 isolated the `H22` catastrophe with the dial: *StringBuilder alone is fine,
AbstractStringBuilder alone is fine, together they break.* At registration level,
with every door closed:

| retired | passed | result |
|---|---:|---|
| `StringBuilder` | 107 | clean |
| `AbstractStringBuilder` | 107 | clean |
| `StringBuffer` | 107 | clean |
| `StringBuilder` + `AbstractStringBuilder` | **95** | **12 fails** |
| all three | **95** | **12 fails** |

The isolation is real and not a dial artifact.

### 4.2 `H22`'s description of the failure is wrong

`H22`, quoted by this brief: *"every `append` is silently discarded and
`toString()` returns empty."* MEASURED, both retired:

```text
length=4      capacity=16   charAt0=a    charAt3=7
substr=abc7   indexOf=2     chars.count=4
field value = byte[16] first=97 98 99 55      field count = 4    field coder = 0
toString.len=0
```

**Nothing is discarded.** The appends land, the byte payload is correct
(`97 98 99 55` = `abc7`), `count` and `coder` are correct, and every accessor
except one is correct. `toString()` alone fails. A remedy aimed at "appends are
discarded" would have been aimed at nothing.

### 4.3 The `sb_only` / `sbuf_only` clean arms are a TRAP — do not bank them

`sb_only` is 107/107 for **−7 census rows**, against 45 rows in the block. It
looks like a free 45-row win and is not one: with `AbstractStringBuilder`'s
registrations still present, dispatch on a `StringBuilder` receiver is absorbed
by the **superclass's** natives. Nothing runs real bytecode; the same native code
services the same calls through a different registration.

This is trap 4 with a new face — *retiring a class promotes its superclass* —
and it is worse than the duplicate-registration form, because the census **does**
fall by 7, so it scores as a win. Taking it would make the number better and the
contract no better at all.

**ARGUED, and the discriminator is cheap:** the same 12 vectors fail the moment
`AbstractStringBuilder` is added, which is what "the superclass was doing the
work" predicts. Worth confirming by dumping which registrar services a
`StringBuilder.append` under `sb_only` before anyone relies on it.

### 4.4 The blocker, localized to one native

`javap -p -c --system <jdk-25> java.lang.StringBuilder` — MEASURED, JDK 25:

```text
public java.lang.String toString();
   1: invokevirtual  Method length:()I
   4: ifne  10
   7: ldc   String ""                    // empty-builder fast path
  16: invokespecial Method java/lang/String."<init>":(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V
```

`length()` returns 4, so the fast path is not taken and the call lands on
`String.<init>(AbstractStringBuilder, Void)` — **a registered native**,
`native-builtins/src/lib.rs:11269`, `owns_slot: true`, whose own registration
comment states the premise:

> *`Intrinsic` for the same load-bearing reason: **this VM's builders are
> `char[]`-backed** and the real ctor's `Arrays.copyOfRange` over a `byte[]`
> reads them one byte at a time.*

Confirmed by reading the field back: with the natives **present**,
`AbstractStringBuilder.value` is a **`char[]`**; with them retired, real bytecode
produces the JDK 25 **`byte[]` + `coder`**. Two layouts for one field, and a
native that only knows the older one.

Every link of the chain works when driven directly under retirement —
`Arrays.copyOfRange` → 4 bytes, `new String(byte[],byte)` → `abc7`,
`StringLatin1.newString` → `abc7`, `isLatin1()` → true. Only the whole call does
not.

### 4.5 Resolved — `StringBuilder.toString()` has THREE doors, and they disagree

The first-call/second-call split in §4.4 is not a latch. Instrumented at the
producer (`native_sb_to_string` and `sb_state`, which is where the truncating
`count` comes from), with both builder classes retired:

```text
[w3sb] sb_state fields=6 buf_is_char=false count_slot=Some(3) count=8 …
LATIN1   s.length()=0              <- FIRST toString(): no to_string line at all
[w3sb] sb_state … count=8
[w3sb] to_string count=8 chars_len=8
LATIN1   secondCall.length()=8     <- SECOND toString(): served by the intrinsic
```

**The first call never reaches `native_sb_to_string`.** The second does. Same
receiver, same method, two consecutive calls, two different implementations —
and the two disagree about the answer.

Closing the second door as well identifies it. `CRATONVM_DISABLE_INTRINSICS=1`
(the interpreter intrinsic table, `native-builtins/src/intrinsics/mod.rs:80`,
which binds `StringBuilder.toString`/`length`/`append` **directly to the same
Rust bodies without consulting the registry**) drops the instrumented call count
from 12 to 4 and makes the answer consistent:

| doors closed | LATIN1 1st | LATIN1 2nd | UTF-16 |
|---|---:|---:|---:|
| none (control) | 8 | 8 | 7 |
| registrations only | **0** | 8 | 7 |
| registrations + intrinsics | **0** | **0** | **0** |

With both doors shut the answer is empty *consistently*, which is the honest
reading: **real JDK `AbstractStringBuilder` bytecode does not produce a usable
string on this VM**, and the natives and intrinsics were both masking it. The
"first call only" and "UTF-16 is fine" effects were two doors interleaving, not
two bugs.

A third door remains open: `sb_state` is still entered 4 times with **both**
the registrations and the intrinsic table disabled, so `length()` reaches a
native body by a route neither switch controls.

**This is trap 2 generalized, and it is the most important thing in this note.**
The brief says an armed zero is unreliable because the dial reaches one dispatch
door. The measurement says the doors are not merely several — they are
*independent registrars of the same method*, at least three of them, only one of
which the census counts and only one of which any "retirement" reaches.

## 5. REFUSED — the 30-row exception retirement, with evidence

§2 prices `exceptions` at 107/107 and −30 census, and `H14-2` §5 predicts it
(21 of 22 registered on a class that does not declare the method). It is still
refused, because the arm retired a **class** and the source holds something else.

MEASURED, `--dump-native-registry`, the 21 `java/lang/*Exception|*Error` classes:

| | |
|---|---:|
| registrations on those classes | **651** |
| registrars they span | **3** (`lang_misc.rs` 576, `lib.rs` 74, `reflect_annotations.rs` 1) |
| already `owns_slot: false` — losers a deletion would PROMOTE | **66** |

The gate suppressed all 651 at once, which no single source edit does. Deleting
the `lib.rs` extras loop promotes 66 already-condemned bodies into service —
`H22` nearly shipped exactly that mistake at a scale of 16, and this is 66.

**Work-order for whoever takes it**, in this order and not another:

1. Extend the gate from classes to `class#method#descriptor` triples.
2. Price the `lib.rs` extras loop **alone** (74 rows), not the 21 classes.
3. `--dump-native-registry` before/after, diffing `owns_slot` — the pass
   condition is *zero* rows flipping `false → true`, not a green suite.
4. Only then delete, and re-run all three arms.

## 6. Reproducing the instrument

Not landed: `CRATONVM_`-prefixed flags are a guarded surface with five
generated/checked artifacts (`types/tests/flag-surface.txt`,
`flag_declaration_guard.rs`, `flag_docs_generated.rs`, `flag_surface.rs`,
`flag_env_mutation_guard.rs`), and doing that properly is its own pass. The
patch is four lines at one choke point, `native-api/src/registry.rs`, at the top
of `NativeMethodRegistry::register`:

```rust
if !w3_retired_classes().is_empty() && w3_retired_classes().iter().any(|c| c == class_name) {
    return;
}
```

with a memoised reader (an unset var costs one atomic load and an `is_empty`,
consulted ~10,000 times a boot):

```rust
fn w3_retired_classes() -> &'static [String] {
    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| match std::env::var("W3_RETIRE") {
        Ok(v) => v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Err(_) => Vec::new(),
    }).as_slice()
}
```

Landing it is worth a pass of its own: WORKER 2's collection prefixes, WORKER 4's
`java.io` block and `H0-4`'s six priced registrars are all sitting on the same
unanswerable question this makes cheap.

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-4` — a registration-level gate prices what the dial cannot; 19
  `java.lang` groups priced against one binary; only 34 of 168 core rows are free
* `WORKER-3-NOTE-4` §4.2 — `H22`'s "every append is silently discarded" is
  wrong: appends land, `count`/`coder`/payload are correct, `toString()` alone fails
* `WORKER-3-NOTE-4` §4.3 — retiring a class can PROMOTE its superclass's
  natives: `sb_only` scores −7 census while changing nothing
* `WORKER-3-NOTE-4` §4.5 — `StringBuilder.toString()` has at least THREE
  independent doors (registry, interpreter intrinsic table, and one neither
  switch reaches) and they return different answers for the same call
* `WORKER-3-NOTE-4` §5 — the 30-row exception retirement is REFUSED: the arm
  retired a class, the source spans 3 registrars and 651 rows, and deleting any
  of them promotes 66 `owns_slot: false` losers
