# E21-1 — `GETSTATIC` has no native path: the enum constants converted to the `<clinit>` shape, the file that said it had already deleted its dead rows, and the true size of the family (148, not 5)

**2026-08-13, lane E21.** Answers **N1** and **N3** of
`E13-1-the-six-builders-decode-a-reference-as-an-int.md`. Patches
`native-builtins/src/phases_late/net_channels.rs` and
`native-builtins/src/http_client.rs`, which this lane owns. The patches are
applied and in the working tree.

**This lane may not build or run the VM, and did not.** Every JDK fact below is
from `javap`/`java` on this host (Microsoft build 25.0.3+9-LTS) and is quoted.
Every claim about CratonVM's behaviour — before and after — is **PREDICTED**
from source. **The seven Rust tests this lane added have never been executed.**
Both files were parse-checked (`rustfmt --edition 2021 --emit stdout` on scratch
copies, exit 0), which rules out syntax errors and nothing else; neither was
type-checked.

---

## 0. Verdict

| claim | verdict |
|---|---|
| E13-1 N1: this VM has no `getstatic`-to-native path | **CONFIRMED, and it is three paths, not one** (§1) |
| `stack_walker.rs:381-427` is the `<clinit>` shape to copy | **CONFIRMED** — copied, with two corrections to it (§2, §7 N3) |
| the five `net_channels.rs` enum rows were dead | **CONFIRMED** — converted; the enum is covered whole, with `values()`/`valueOf` (§2) |
| "if `getstatic` can never reach a native, EVERY field-shaped registration is dead" | **TRUE, and the family is 148** — but only ~102 of them were ever reachable even in principle, and the reason is measured (§4) |
| E13-1 N3: eight unaudited `Optional` sites in `http_client.rs` | **AUDITED. Two of the eight are wrong; six are correct.** The SHAPE sweep finds **three more** the `Optional` framing does not (§5) |
| the family in `http_client.rs` counted by shape | **FIVE**, not eight-of-which-some (§5) |
| slot collisions activated by the fix | **NONE — audited, and one field's default had to change instead** (§5c) |
| `mint-version-present-name` can now go green | **NOT YET.** This lane closed the native half; the `classloading` half is nominated and unlanded (§3) |

---

## 1. THE CLAIM, RE-VERIFIED — and it is worse than E13-1 said

E13-1 §7 named two static-read paths. There are **three**, and the third is
further from a registry lookup than either:

| path | what it does with the field | registry lookup? |
|---|---|---|
| `vm/src/runtime/interpreter/opcodes.rs:1205-1367` `Instruction::Getstatic` | resolve → JVMTI `fire_field_access_if_watched_for_vm` → `System.out/err/in` intercept → `java/lang/Boolean` `TRUE`/`FALSE` special case → `get_static_shared` | **no** |
| `vm/src/jit/helpers.rs:6541` `jit_getstatic` | the same intercepts, then `get_static_shared` | **no** |
| `jit/src/ir_lower.rs:2238` `emit_inline_getstatic` / `x64::try_emit_inline_getstatic`, resolved by `jit/helpers.rs:6503 jit_resolve_static_base` | **bakes the statics-base address as an immediate and emits two dependent loads** — no helper call at all | **no, and no call** |

`Instruction::Getstatic` is matched in exactly one place
(`grep -rn "Instruction::Getstatic" vm/src` → `opcodes.rs:1205` plus a stack-shape
row at `exceptions.rs:844`), so the interpreter does NOT have the two-handlers
hazard for this opcode — but the JIT does have two lowerings for it, and the
faster one is a pair of `mov`s against a baked address. **No amount of
registry work can be seen from there.** A "just add a getstatic hook" repair
would have to touch all three, and the third would lose the optimisation that
exists because `getstatic` cost ~35 ns against HotSpot's ~1.

The shape that IS supported: `vm/src/vm/vm_util.rs:1228-1253` computes
`has_clinit` as *the class declares `<clinit>()V`* **OR**
`shared.natives.native_methods.find(&class_name, "<clinit>", "()V").is_some()`,
and then `invoke_on_class_shared(..., "<clinit>", "()V", &[])` runs it exactly
once per class. After that, plain `getstatic` reads what it published. That is
the whole mechanism.

`stack_walker.rs:381-427` (`native_option_clinit`) is exactly as described:
name-string then instance, `Enum`-resolved `name`/`ordinal` slots, publish to
the static, then a second pass that **re-reads each constant out of its static**
to fill `$VALUES` — so `values()[i] == CONSTANT`. Its doc comment already names
the trap this lane was warned about, in the same words the netty finding used.

## 2. WHAT LANDED IN `net_channels.rs`

The five rows at `:1033-1065` are gone. In their place, per enum: a
`<clinit>`, a `values()` and a `valueOf(String)`.

### 2a. The enum is covered whole, and the shape came off `javap`, not off memory

```
$ javap -p java.net.http.HttpClient$Version
public final class java.net.http.HttpClient$Version extends java.lang.Enum<...> {
  public static final ... HTTP_1_1;      public static final ... HTTP_2;
  private static final ...[] $VALUES;
  public static ...[] values();          public static ... valueOf(java.lang.String);
}
$ javap -p java.net.http.HttpClient$Redirect   ->  NEVER; ALWAYS; NORMAL;
$ java  E21Enum
VER HTTP_1_1 ord=0 ... VER HTTP_2 ord=1 ...     RED NEVER 0 / ALWAYS 1 / NORMAL 2
valueOf-identity true   values-identity true    values-fresh-array true
compareTo -1            enummap h2 size=1       enumset [NEVER, ALWAYS, NORMAL]
```

Four of those measured facts are load-bearing and each is encoded in the patch:

1. **Declaration order is the ordinal**, and `Redirect`'s is `NEVER, ALWAYS,
   NORMAL` — *not* alphabetical and not least-to-most permissive. A
   plausible-looking reordering silently changes `compareTo` and `EnumSet`
   iteration.
2. **`values()` returns a FRESH array** (`values() != values()`), while
   `values()[1] == HTTP_2`. Handing back the `$VALUES` array itself would let
   one caller's `values()[0] = null` corrupt every later caller.
3. **`valueOf` resolves through the static**, so `valueOf("HTTP_2") == HTTP_2`.
4. Both failure shapes are the oracle's, quoted: `valueOf(null)` →
   `NullPointerException: Name is null`; `Redirect.valueOf("nope")` →
   `IllegalArgumentException: No enum constant java.net.http.HttpClient.Redirect.nope`
   — the nested class renders with a **dot**, so both `/` and `$` are replaced.

### 2b. THE KNOWN TRAP — `name()` is asserted, not just non-null

E13-1 (c) named it and this session had already paid for it once: a nameless
enum constant makes `Enum.valueOf` match nothing while every null check passes,
and one such constant in one JDK enum zeroed fifteen netty classes.

`http_enum_clinit` writes `name` and `ordinal` **resolved against
`java/lang/Enum`**, never against the receiver's class, with fallbacks `0`/`1`
that match `lang_misc::ENUM_NAME_SLOT`/`ENUM_ORDINAL_SLOT` — the slots
`Enum.name()`/`Enum.ordinal()` actually read. Receiver-scoped resolution is what
`lang_misc::native_enum_name`'s comment records as having made
`Infrastructure.JERSEY.name()` answer `"Jersey"` (an enum may declare its own
`name` field, which shadows `Enum`'s).

**Getting only the fallback pair the wrong way round reproduces the nameless
constant exactly** — which is what §7 N3 nominates against the landed
`PosixFilePermission` model, where `name` falls back to `1` and `ordinal` to `0`.

### 2c. Two deliberate departures from the `stack_walker` model

* **The fresh constant is PINNED across `create_string`.**
  `native_option_clinit` creates the name string *first* and then allocates the
  option, which leaves the string ref live across an allocation — the same
  native-stale-local family, one step to the left. This file's own
  `SelectableChannel.register` already uses
  `pin_native_root`/`read_native_pin`/`unpin_native_roots`, so the patch follows
  the local idiom instead.
* **`is_class_synthetic_stub` gates the mint.** In real-JDK mode
  `java.net.http.HttpClient$Version` is a real class with real `<clinit>`
  bytecode; clobbering the JDK's interned constants with synthetic stand-ins
  would be a regression in the mode that matters most. The cost is stated in
  §6: `MockNativeContext` answers `false`, so this body cannot be driven from a
  Rust unit test at all.

### 2d. The comment that said the dead rows had already been deleted

`net_channels.rs` contained **both** of these, forty lines apart, about the same
four registrations:

* `:694-698` — "Every `SelectionKey` instance method below existed, but nothing
  could name the bits they take: `key.interestOps(SelectionKey.OP_READ)` had no
  way to reach the value 1" — i.e. *these rows fix that*.
* `:743-751` — "**REMOVED** (stub-removal wave 2): `SelectionKey.OP_READ/…` were
  registered as *methods* with the FIELD descriptor `"I"` … dead registrations,
  not stubs."

**The rows were never removed.** They are still at `:699-702` (now `:714-717`),
and the note claiming their removal is the one that carries the correct
diagnosis. Both comments are rewritten to say what is true; the rows are left in
place *only* because deleting them turns `vm/src/vm/tests.rs`'s
`selection_key_constants_p58` red, and that file belongs to another lane (§7 N2).

This is worth carrying forward on its own: **a "REMOVED" note is not evidence of
a removal.** The sweep in §4 found the rows the note said were gone.

## 3. WHAT THIS LANE COULD NOT DO, AND WHY THE FIXTURE ROW IS STILL RED

`set_static_field_by_name` resolves a **declared** static field and is a silent
no-op otherwise. `classloading/src/class_manager.rs` has **no entry at all** for
`java/net/http/*` (`grep -n "java/net/http" classloading/src/class_manager.rs`
→ nothing), so on a synthetic stub:

* `HttpClient$Version`'s superclass is the blanket `java/lang/Object`, not
  `java/lang/Enum` — so it inherits no `name`/`ordinal` slots;
* `HTTP_1_1`/`HTTP_2`/`$VALUES` are not declared, so `GETSTATIC` has no field to
  resolve **and** `http_enum_clinit`'s publishes go nowhere.

**So the native half of N1 is landed and the classloading half is not.** Until
§7 N1 lands, `http_enum_clinit` runs and writes nothing, exactly as
`native_option_clinit` does for `$VALUES` today (§7 N3). `mint-version-present-name`
stays red, and this record does not claim otherwise. The honest order is
unchanged from E13-1 §7: *the constant must exist, then the builder must decode
it, then the accessor must mint the mirror.* Two of three were done before this
lane; this lane did the fourth thing nobody had listed — the constant must be
**reachable**, which is a different requirement from existing.

## 4. TASK 1(d) — THE FULL SWEEP: 148 field-shaped registrations, and only ~102 could ever have mattered

### 4a. Two instruments, each with a blind spot, so the answer is their union

| instrument | field-shaped rows | what it cannot see |
|---|---|---|
| `scratchpad/p1/reg.json` — a live registry dump, `mode: compatible`, 11,748 rows | **42**, and **every one has `invocations: 0`** | everything registered only inside `register_synthetic_overrides` — including all five `HttpClient$Version`/`$Redirect` rows, which do not appear in it at all |
| source sweep — parse every `.register(a, b, c, …)` whose third argument is a string literal not starting with `(` (`scratchpad/e21/sweep2.py`) | **130** (131 hits minus one false positive: a `SubstitutionRegistry::register` inside a `graalvm_compat.rs` test) | rows generated by `macro_rules!`, where the class/name arguments are `$` metavariables — e.g. `locale_bootstrap.rs`'s `locale_const!`, **18 rows**, which the dump does see |

Union: **148**. The set difference works out exactly — the dump's 42 minus the
24 the source sweep also found is precisely locale_bootstrap's 18. Neither
number alone is the answer, and `invocations: 0` is a **necessary, not
sufficient** signature: 88 of the 130 never reach a compatible-mode dump at all.

After this patch: **143**.

### 4b. Which of the 148 are worth converting — a measured discriminator, not a taste call

`javac` **inlines** a `static final int` whose initialiser is a compile-time
constant expression (JLS §13.1); no `getstatic` is ever emitted. Measured on
this host:

```
$ javap -c E21D
  static int a();  0: iconst_1            //  SelectionKey.OP_READ
  static int b();  0: sipush 200          //  HttpURLConnection.HTTP_OK
  static int c();  0: bipush 16           //  Spliterator.ORDERED
  static Object d(); 0: getstatic  TimeUnit.SECONDS:Ljava/util/concurrent/TimeUnit;
  static Object e(); 0: getstatic  StandardCharsets.UTF_8:Ljava/nio/charset/Charset;
  static Object f(); 0: getstatic  File.separator:Ljava/lang/String;
  static char   g(); 0: getstatic  File.separatorChar:C
  static Object h(); 0: getstatic  java/time/Duration.ZERO:Ljava/time/Duration;
```

So the 46 primitive-descriptor rows split:

* **43 are dead twice over** — compile-time constant `int`s that no compiled
  caller ever `getstatic`s. Converting them to `<clinit>` buys nothing for
  bytecode; only reflection could reach them. They should be **deleted**, not
  converted.
* **2 really do `getstatic`** — `File.separatorChar` / `pathSeparatorChar` are
  `char`s assigned from a method call, so they are not constant expressions.
  Both are already served by a `<clinit>` backfill in `vm_util.rs` (the
  HIB-CV-27 hook), which is what makes the registrations redundant rather than
  merely dead.
* **1 is nonsense** — `java/io/Serializable.serialVersionUID` with descriptor
  `"J"` (`phases_late/collections.rs:2169`), on an interface.

That leaves **102 reference-typed rows**, and those are the ones an app can
actually reach. Triage:

| rows | constants | judgement |
|---|---|---|
| **22** | `java/util/Locale.*` (`locale_bootstrap.rs` ×18, `lib.rs` ×4) | **CONVERT — highest value.** `Locale.US` is read by getstatic from formatting, collation and `toLowerCase` paths everywhere |
| **14** | `java/time`: `Duration.ZERO`, `Instant.EPOCH`, `LocalTime.{MIDNIGHT,NOON,MIN,MAX}`, `ZoneOffset.UTC`, `Period.ZERO`, `DateTimeFormatter.ISO_*` (`util_time.rs`) | **CONVERT.** `Duration.ZERO` is measured above as a real `getstatic` |
| **7** | `java/util/concurrent/TimeUnit.*` (`concurrent.rs`) | **CONVERT — highest value per row.** `TimeUnit.SECONDS` is measured above; a *different* defect in this same enum (`phases_early::tu_ordinal` decaying to ordinal 0) already turned `SECONDS.toNanos(1)` into `1` |
| **6** | `java/nio/charset/StandardCharsets.*` (`charset_buffers.rs`) | **CONVERT.** measured above |
| **10** | `java/lang/foreign/{MemorySegment.NULL, ValueLayout.*}` (`foreign_ffm.rs`) | **DO NOT CONVERT.** `prepare_class` already pre-seeds these statics and `vm_util.rs` documents an FFM bootstrap guard around them; a second publisher would fight it |
| **7** | `java/lang/System$Logger$Level.*` (`phases_late.rs`) | convert — cheap, and `System.Logger` is a real app surface |
| **3** | `java/lang/StackWalker$Option.*` (`phases_late.rs:5665-5684`) | **DELETE — a duplicate-fix shadow.** `stack_walker.rs` already publishes these three through a `<clinit>`. The field-shaped rows survived the fix that superseded them |
| **4** | `java/util/jar/Attributes$Name.{MANIFEST_VERSION,MAIN_CLASS}` — registered **twice**, in `lib.rs` and `jar_manifest.rs` | delete one pair, convert the other |
| **4** | `java/math/MathContext.DECIMAL{32,64,128}`, `UNLIMITED` | convert |
| **4** | `java/nio/ByteOrder.{BIG,LITTLE}_ENDIAN` — **also registered twice** (`phases_early.rs`, `servlet.rs`) | delete one pair, convert the other |
| **4** | `java/nio/file/WatchEvent$Kind.*` (`nio_file.rs`) | convert with the `WatchService` work, not before |
| **2** | `java/io/File.{separator,pathSeparator}` | already served by `vm_util`'s backfill — **delete** |
| **4+2+3** | `Normalizer$Form`, `NumberFormat$Style`, `StringTemplate$Processor` | low value; `StringTemplate` is a withdrawn preview API |
| **5** | **`HttpClient$Version`/`$Redirect`** | **DONE (this patch)** |

**The single most useful thing in this table is `TimeUnit`** — seven rows, an
enum whose constants app code reads by `getstatic` on essentially every
concurrent call, and an enum this codebase has *already* been bitten by from the
other direction.

## 5. TASK 2 — `http_client.rs`, counted by SHAPE

E13-1 N3 asked about "eight unaudited 1-slot `Optional` sites". All eight were
read. **The `Optional` framing is the wrong denominator**, exactly as E13-1 §3
found next door when six became ten: the defect is *a native that answers in a
form its descriptor does not name*, and three of this file's instances are not
`Optional`s at all.

### 5a. The eight `Optional` sites — two wrong, six correct

| site | payload it wrote | verdict |
|---|---|---|
| `HttpRequestImpl.version()` | `HRQ_VERSION`, an **`Int`** | **WRONG.** `Value::Int(1)` is not null to `ref_operand_is_null`, so `isPresent()` was TRUE for every request ever built and `get()` returned a primitive typed `HttpClient$Version` |
| `HttpRequestImpl.timeout()` | `HRQ_TIMEOUT_MS`, a raw **`Long`** millis | **WRONG.** `Long(0)` *does* read as null, so the ABSENT arm was accidentally right and only the PRESENT arm was broken — which is why no registration test and no absent-case fixture row could ever see it |
| `HttpResponseImpl.previousResponse()` | `HRS_PREVIOUS`, a reference | correct |
| `HttpClientImpl.executor()` / `.proxy()` / `.cookieHandler()` / `.authenticator()` | reference slots, always `Object(None)` | correct — honestly empty; nothing in the tree ever stores one |
| `HttpClientImpl.connectTimeout()` | built a real `Duration` | correct (E2-1 read this one and used it as its model) |

**Right arity was not evidence of the right type** — E2-1's own point, arriving
here as two of eight.

### 5b. The three the `Optional` framing does not mark — so the family is FIVE

| site | descriptor | what it returned |
|---|---|---|
| `HttpClientImpl.version()` | `()Ljava/net/http/HttpClient$Version;` | `Value::Int` straight out of `HCI_VERSION` |
| `HttpClientImpl.followRedirects()` | `()Ljava/net/http/HttpClient$Redirect;` | `Value::Int` straight out of `HCI_FOLLOW_REDIRECTS` |
| `HttpResponseImpl.version()` | `()Ljava/net/http/HttpClient$Version;` | `Value::Int` straight out of `HRS_VERSION` |

All three hand a primitive to bytecode that is about to `areturn`/`checkcast` a
reference. They are the same defect as the two above with the `Optional`
stripped away — and **`grep 'Some(Value::Int(n))'`, the idiom E2-1 counted by,
does not match a single one of the five**, because this file's defect is on the
*answer* side, not the argument side.

All five now resolve the constant **through the class's static field**
(`enum_constant_static`), so the answer is the same object `GETSTATIC` yields —
`client.version() == HttpClient.Version.HTTP_2`, and
`client.version() == client.version()`. Minting per call (the `p57_alloc_enum`
shape the deleted registrations used) satisfies neither, and an enum whose
constants fail `==` breaks `EnumMap`, `EnumSet` and every `switch`. **This is
the direct payoff of §2: the accessors are consumers of the `<clinit>` this
patch installed in the sibling file.**

### 5c. SLOT COLLISIONS — audited before, not after

E13-1's warning was that a patch which starts populating a field detonates a
collision that was dormant. Audit of every slot this patch makes live:

* `HRQ_VERSION` (slot 5) — **one writer** (`<init>`), **one reader**
  (`version()`), in the whole tree. `do_send` reads the version off the CLIENT
  (`HCI_VERSION`), never off the request. No collision.
* `HRQ_TIMEOUT_MS` (slot 4) — one writer (`<init>`, `Long(0)`), one reader.
* `HCI_VERSION` / `HCI_FOLLOW_REDIRECTS` — written by `hci_init`, read by
  `do_send` **and** by the two accessors. The accessors were changed; the
  **stored encoding was not**, precisely so `do_send`'s wire behaviour is
  bit-identical.
* `grep -rn "HttpClientImpl|HttpRequestImpl|HttpResponseImpl"` outside this file
  returns four hits, all comments except `tls_impl.rs:1369`, which adds a
  `tlsVersion()` method and touches no slot.

**One default had to change, and it is the opposite hazard.** The oracle says
`HttpRequest.newBuilder(uri).build().version()` is `Optional.empty` — a request
carries a version only if one was set. `<init>` stored `Int(HTTP_VERSION_2)`,
and this file's `HTTP_VERSION_1_1 = 0`, so `0` was **not free** as the "unset"
encoding. A new sentinel `HRQ_VERSION_UNSET = -1` is introduced and `<init>`
writes it. This is safe only because of the one-writer/one-reader audit above.

**The two files encode the same concept differently and both are internally
consistent**: `http_client.rs` stores the JDK ordinal (`HTTP_1_1 = 0`), while
`http2.rs` stores *ordinal + 1* so that `0` can mean "no override". Nothing
hands one file's stored int to the other today. The conversion happens at the
boundary in `http_version_mirror`, and a test pins both encodings so a
"tidy-up" that unifies them has to read the reason first.

### 5d. A MEASURED DIVERGENCE THIS PATCH DELIBERATELY DOES NOT FIX

`hci_init` stores `REDIRECT_NORMAL`, but on JDK 25.0.3+9-LTS **both**
`HttpClient.newHttpClient().followRedirects()` and
`HttpClient.newBuilder().build().followRedirects()` answer **`NEVER`**.

Correcting the default is not a type fix: `do_send` feeds
`HCI_FOLLOW_REDIRECTS` to `perform_request`, so flipping it stops this VM
following redirects on every request that never asked. That is a wire-behaviour
change with its own blast radius and it belongs in its own patch with its own
evidence. It is recorded here and in a comment at the site — not folded in.
(Also note this file's `REDIRECT_*` order is `NEVER, NORMAL, ALWAYS`, which is
**not** the JDK's `NEVER, ALWAYS, NORMAL`; `http_redirect_mirror` remaps rather
than renumbers, for the same reason.)

### 5e. Two more corrections that came free

* **`Duration` normalisation.** `connectTimeout()` computed `ms / 1000` and
  `(ms % 1000) * 1e6`. A real `Duration.ofMillis(-1500)` is `seconds = -2,
  nanos = +500_000_000` (E13-1 measured it; `nanos` is documented non-negative).
  The truncating form produced `seconds = -1, nanos = -500_000_000`, which no
  JDK `Duration` method is prepared for. Both `Duration`-minting sites now go
  through one `div_euclid`/`rem_euclid` helper.
* **The GC window.** That helper pins the `Optional` across the `Duration`
  allocation. E2-1 declined to pin *some* of `http2.rs`'s sites on the grounds
  that a half-pinned file is worse than an unpinned one; here **both** members
  of the family route through the one helper, so the file has no half-pinned
  pair.

## 6. VERIFICATION SCOPE — stated honestly

### 6a. Seven new Rust tests, none of them ever executed

`http_client.rs` had 22 tests. Every one that touches a native is
registration-only (`find(...).is_some()`), so all 22 stayed green through both
defect families — the same finding E2-1 made about `http2.rs`'s 64. The seven
added here invoke through the registry and assert the slot actually written:

| test | covers |
|---|---|
| `e21_request_version_is_empty_and_never_a_primitive` | the `<init>` sentinel + the empty arm |
| `e21_request_timeout_present_is_a_duration_not_a_long` | the arm that was wrong AND the arm that was accidentally right |
| `e21_connect_timeout_duration_is_floor_normalised` | `-1500 ms` → `(-2, +5e8)` |
| `e21_presence_accessors_are_empty_optionals_not_flags` | **negative control** — the four that were already correct |
| `e21_enum_accessors_never_return_an_ordinal` | all three of §5b, as a law |
| `e21_previous_response_is_an_empty_optional` | the site C12-3 calls "the one that settles what this is", still covered by nothing else |
| `e21_this_files_version_constants_are_the_jdk_ordinals` | the two encodings, pinned against a unifying tidy-up |

A shared `e21_assert_optional` fails with the diagnosis rather than a bare
inequality, and asserts `object_num_fields(opt) == 1` — the mock zero-fills to
`Value::Int(0)`, so a native that wrote *nothing* fails the same way the bug
did.

### 6b. WHAT THE MOCK CANNOT MEASURE — and it is the interesting half

`MockNativeContext` has **no statics table at all**:

```
native-api/src/registry.rs:4303  fn static_field_index_by_name(...) -> Option<usize> { None }   // default
native-api/src/test_mock.rs:814  fn get_static_field(...) -> Value { Value::Int(0) }
native-api/src/test_mock.rs:817  fn set_static_field(...) {}                                     // no-op
native-api/src/registry.rs:561   fn is_class_synthetic_stub(...) -> bool { false }               // default, not overridden
```

Consequences, both of which this record refuses to paper over:

1. **`http_enum_clinit` cannot be unit-tested at all.** The stub gate returns
   `false` under the mock, so the body never runs; and even without the gate,
   every publish is a no-op and every read-back answers `Int(0)`. A test written
   against it would measure the mock — the exact failure mode
   `MockNativeContext::get_field_by_name`'s own doc comment records two bugs
   for. **No such test was written.** The three `net_channels` registrations are
   covered by nothing but the registry.
2. **`enum_constant_static` can only reach its fail-safe** in
   `e21_enum_accessors_never_return_an_ordinal`. That test therefore pins the
   *law* ("never a primitive where a reference is declared") and not the mirror
   lookup — which is still the exact discriminator, because all three sites
   returned `Value::Int` before. The mirror needs a VM; §7 N4 nominates the mock
   upgrade that would let a unit test see it.

### 6c. Fixture coverage is unchanged, and is still ZERO for both files

`register_p60_http_client` is reachable only from
`register_synthetic_overrides` (`native-builtins/src/lib.rs:24071` →
`register_phase60_natives`), which is `#[cfg(feature = "synthetic-jdk")]` and
runtime-gated. `reg.json`, a compatible-mode dump, contains **37 rows from
`net_channels.rs` and not one of them is from this registrar** — independent
confirmation of the gate. `RJdkOptionalShape` runs in `CORE_CLASSES`, default
mode, where `net_phase_e.rs` answers. E13-1 §4b's conclusion stands unchanged
and `run.sh:138-163` already records it: **`--synthetic-jdk` is not a flag on
this binary**, so gating this needs a second binary. No fixture arm was added,
for the reason E13-1 gives — it would be a gate that cannot fail or one that
fails every run.

## 7. NOMINATIONS

### N1 — `classloading/src/class_manager.rs` (NOT this lane's file): without this, §2's `<clinit>` publishes into the void

Three edits, all modelled on the landed `PosixFilePermission` entry.

**(a) the superclass edge** — without it the stub extends `java/lang/Object`,
inherits no `name`/`ordinal` slots, and `Enum.name()` reads a slot that does not
exist.

*exact literal old text* (≈`:10783`):
```
        // java.nio.file.attribute — enum PosixFilePermission extends Enum
        "java/nio/file/attribute/PosixFilePermission" => "java/lang/Enum",
```
*exact literal new text:*
```
        // java.nio.file.attribute — enum PosixFilePermission extends Enum
        "java/nio/file/attribute/PosixFilePermission" => "java/lang/Enum",

        // java.net.http — HttpClient's two nested enums. Their constants are
        // published by a native <clinit> in
        // native-builtins/src/phases_late/net_channels.rs; without this edge
        // the stub extends Object, inherits no name/ordinal slots, and every
        // constant is nameless (see E21-1 §2b).
        "java/net/http/HttpClient$Version" | "java/net/http/HttpClient$Redirect" => {
            "java/lang/Enum"
        }
```

**(b) the static field declarations** — `set_static_field_by_name` resolves a
declared field and is a silent no-op otherwise, and `GETSTATIC` needs a field to
resolve.

*exact literal old text* (≈`:13434`):
```
        // Spring Boot 3 JarFileArchive.<clinit> reads PosixFilePermission.OWNER_* statics.
```
*exact literal new text:*
```
        // java.net.http.HttpClient's two nested enums. `$VALUES` is declared
        // alongside the constants deliberately: `StackWalker$Option`'s entry
        // omits it, so that model's `$VALUES` publish is a silent no-op today
        // (E21-1 §7 N3). Values are wired from a native <clinit> — see
        // `http_enum_clinit` in phases_late/net_channels.rs.
        "java/net/http/HttpClient$Version" => {
            let mk = |n: &'static str, d: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc(d),
                attributes: vec![],
            };
            vec![
                mk("HTTP_1_1", "Ljava/net/http/HttpClient$Version;"),
                mk("HTTP_2", "Ljava/net/http/HttpClient$Version;"),
                mk("$VALUES", "[Ljava/net/http/HttpClient$Version;"),
            ]
        }
        "java/net/http/HttpClient$Redirect" => {
            let mk = |n: &'static str, d: &'static str| ClassFileField {
                access_flags: FieldAccessFlags::PUBLIC
                    | FieldAccessFlags::STATIC
                    | FieldAccessFlags::FINAL,
                name: cratonvm_types::intern_arc(n),
                descriptor: cratonvm_types::intern_arc(d),
                attributes: vec![],
            };
            vec![
                mk("NEVER", "Ljava/net/http/HttpClient$Redirect;"),
                mk("ALWAYS", "Ljava/net/http/HttpClient$Redirect;"),
                mk("NORMAL", "Ljava/net/http/HttpClient$Redirect;"),
                mk("$VALUES", "[Ljava/net/http/HttpClient$Redirect;"),
            ]
        }

        // Spring Boot 3 JarFileArchive.<clinit> reads PosixFilePermission.OWNER_* statics.
```

**(c) the `<clinit>` method declaration** — strictly optional (`vm_util.rs`
falls back to the native registry when the class declares no `<clinit>`), but
both landed models declare it and the belt-and-braces costs nothing.

*exact literal old text* (≈`:15848`):
```
    if name == "java/nio/file/attribute/PosixFilePermission" {
```
*exact literal new text:*
```
    if name == "java/net/http/HttpClient$Version" || name == "java/net/http/HttpClient$Redirect" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "java/nio/file/attribute/PosixFilePermission" {
```

**Do NOT also declare `values`/`valueOf` here** — `phases_late/net_channels.rs`
registers both natives, and `vm_util`'s registry fallback is what selects them.

### N2 — `vm/src/vm/tests.rs` (NOT this lane's file): two tests now name registrations that no longer exist

`http_version_enums_p60` (≈`:46095`) and `http_redirect_enums_p60` (≈`:46122`)
`call_native` the five deleted field-shaped rows. **Merging this lane's patch
without this edit turns both red.** They are also the shape E13-1 named: a test
that keeps a dead registration alive by being the only thing that can reach it.

*exact literal old text* (the first, verbatim from `http_version_enums_p60`):
```
        let v1 = call_native(
            &shared,
            &mut thread,
            "java/net/http/HttpClient$Version",
            "HTTP_1_1",
            "Ljava/net/http/HttpClient$Version;",
            &[],
        )
        .unwrap()
        .unwrap();
        assert!(v1.as_object().is_some());
        let v2 = call_native(
            &shared,
            &mut thread,
            "java/net/http/HttpClient$Version",
            "HTTP_2",
            "Ljava/net/http/HttpClient$Version;",
            &[],
        )
        .unwrap()
        .unwrap();
        assert!(v2.as_object().is_some());
```
*exact literal new text:*
```
        // E21-1: the FIELD-shaped registrations these lines used to call are
        // gone — no GETSTATIC path in this VM consults the native registry, so
        // they were reachable only from here. The constants are now published
        // by a native `<clinit>`, which is the shape bytecode can actually
        // reach, and `values()` is the reachable accessor.
        call_native(
            &shared,
            &mut thread,
            "java/net/http/HttpClient$Version",
            "<clinit>",
            "()V",
            &[],
        )
        .unwrap();
        let vals = call_native(
            &shared,
            &mut thread,
            "java/net/http/HttpClient$Version",
            "values",
            "()[Ljava/net/http/HttpClient$Version;",
            &[],
        )
        .unwrap()
        .unwrap();
        assert!(vals.as_object().is_some());
```
and the identical treatment for `http_redirect_enums_p60`, with
`java/net/http/HttpClient$Redirect` and
`"()[Ljava/net/http/HttpClient$Redirect;"`.

**This assertion is deliberately weak until N1 lands** — with no declared
statics, `values()` returns an array of nulls. Once N1 is in, the test that is
actually worth having reads `HTTP_2` out of the static and asserts
`name()` is `"HTTP_2"`, because a non-null constant with a null name is the
failure mode §2b exists to prevent.

Bundled with the same edit, if that lane wants it: deleting
`net_channels.rs:714-717` (the four `SelectionKey.OP_*` rows, §2d) requires
deleting `selection_key_constants_p58` (≈`:44337`). Those four are dead twice
over — §4b measures `SelectionKey.OP_READ` compiling to `iconst_1`.

### N3 — `native-builtins/src/phases_late/nio_file.rs` (NOT this lane's file): the model's `name`/`ordinal` fallbacks are inverted

`posix_file_permission_stub_clinit` (≈`:19630`) resolves the two Enum slots
against **`P`, the receiver class**, with fallbacks the wrong way round:

```rust
    let ord_idx = ctx.resolve_field_index(P, "ordinal").unwrap_or(0);
    let name_idx = ctx.resolve_field_index(P, "name").unwrap_or(1);
```

`lang_misc` defines `ENUM_NAME_SLOT = 0` and `ENUM_ORDINAL_SLOT = 1`, and
`Enum.name()` reads slot 0. So **if resolution ever misses, this writes the
ordinal where `name()` looks** — a nameless enum constant, the exact defect that
zeroed fifteen netty classes. It is latent today only because
`class_manager.rs:10784` gives the stub `java/lang/Enum` as its superclass so
the lookup succeeds; it is one missing superclass row from firing, and any enum
copied from this model without that row fires immediately.

*exact literal old text:*
```
    let ord_idx = ctx.resolve_field_index(P, "ordinal").unwrap_or(0);
    let name_idx = ctx.resolve_field_index(P, "name").unwrap_or(1);
```
*exact literal new text:*
```
    // Resolved against `java/lang/Enum`, never the receiver: an enum may
    // declare its own `name` field, which shadows Enum's (see
    // `lang_misc::native_enum_name`). The fallbacks are Enum's declaration
    // order — name 0, ordinal 1 — which is what `Enum.name()`/`ordinal()`
    // read; inverted, they produce a NAMELESS constant that every null check
    // passes and `Enum.valueOf` matches nothing (E21-1 §2b).
    let name_idx = ctx
        .resolve_field_index("java/lang/Enum", "name")
        .unwrap_or(0);
    let ord_idx = ctx
        .resolve_field_index("java/lang/Enum", "ordinal")
        .unwrap_or(1);
```

Same file, separate observation: `posix_file_permission_stub_clinit` publishes
no `$VALUES`, so `PosixFilePermission.values()` and `EnumSet.allOf` have nothing
to read.

### N4 — `native-api/src/test_mock.rs` (NOT this lane's file): no statics table means no `<clinit>` is testable

`MockNativeContext::set_static_field` is a no-op, `get_static_field` answers
`Value::Int(0)`, and `static_field_index_by_name` falls through to the trait's
`None`. **Every `<clinit>`-shaped native in this repository is therefore
untestable in Rust** — `native_option_clinit`, `posix_file_permission_stub_clinit`
and now `http_enum_clinit` are covered by registry-presence assertions and
nothing else. A `HashMap<(ClassId, String), Value>` plus a
`declare_static_fields(class, &[names])` helper would make §6b's whole second
half testable, and would let a test catch N3's inverted fallbacks by name.

`is_class_synthetic_stub` returning the trait default `false` is the second
half of the same gap: any native that gates on it is inert under the mock.

### N5 — `docs/known-issues/jdk-only/INDEX.md` (NOT this lane's file)

Add:
```
* `E21-1-getstatic-has-no-native-path.md` — three static-read paths, none of
  which consults the native registry; the `HttpClient` enums converted to the
  `<clinit>` shape; 148 field-shaped registrations swept, with the javac
  constant-inlining measurement that says which ~102 could ever have mattered.
```

## 8. Residuals

1. **The seven new tests have never been executed**, and neither file has been
   type-checked. `rustfmt` exit 0 rules out syntax errors only.
2. **§7 N1 is unlanded, so §2's `<clinit>` currently publishes nothing.** The
   native half is necessary and not sufficient — the same shape E13-1 ended in,
   one layer down.
3. **`http_client.rs`'s natives are registered on `jdk/internal/net/http/*`,
   which are REAL JDK classes**, and none of them checks the receiver's shape
   before indexing its slots — `hci_init` writes eight slots into whatever it is
   handed. `http2.rs`'s `is_synthetic_shape` is the model for fixing this; it is
   pre-existing, unchanged by this patch, and it is the largest remaining
   hazard in the file.
4. **`followRedirects`' default is `NORMAL` where the oracle says `NEVER`**
   (§5d). Measured, recorded at the site, deliberately not fixed here.
5. **The two version encodings remain deliberately different** (§5c), pinned by
   `e21_this_files_version_constants_are_the_jdk_ordinals`.
6. **43 compile-time-inlined `int` constants are still registered** (§4b). They
   are the cheapest deletion in the tree and the only reason to keep any of them
   is a `call_native` test.
7. **The source sweep cannot see macro-generated registrations** and the
   registry dump cannot see synthetic-only ones. 148 is the union of two
   instruments with disjoint blind spots; a third registrar shape would raise
   it again.
