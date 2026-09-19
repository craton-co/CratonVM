# E40-1 — the test that pinned a wrong type, the withdrawn class three tests still describe, and 36 test call-sites driving registrations no `getstatic` can reach

**2026-08-13, lane E40.** Answers **N2** and **N5** of
`E36-1-inverted-enum-fallbacks-and-the-field-shaped-rows-that-cannot-fire.md`,
and sweeps `vm/src/vm/tests.rs` for the shapes that record and
`E21-1-getstatic-has-no-native-path.md` named. Patches `vm/src/vm/tests.rs`
and `native-io/src/lib.rs`, which this lane owns. The patches are applied and in
the working tree.

**This lane may not build or run the VM, and did not.** Every JDK fact below is
from `javap`/`java` on this host (Microsoft build 25.0.3+9-LTS) and is quoted.
Every claim about CratonVM's behaviour — before and after — is **PREDICTED**
from source. **The two Rust tests this lane added have never been executed.**
Both files were parse-checked (`rustfmt --edition 2021 --emit stdout` on scratch
copies, exit 0 — `native-io`'s copy needed empty sibling `mod` stubs to resolve),
which rules out syntax errors and nothing else; neither was type-checked.

---

## 0. Verdict

| claim | verdict |
|---|---|
| E36-1 N2(a): `watch_event_kinds_p66` asserts the wrong type is correct | **CONFIRMED, measured, DELETED** — and there was no rewrite available, only deletion (§1a) |
| E36-1 N2(b): `java.lang.StringTemplate` does not exist on JDK 25 | **CONFIRMED. Whole test deleted, going further than the nomination** — the half E36-1 wanted kept contains a second wrong-type native and had no test at all (§1b) |
| E36-1 N2(c): `System$Logger$Level`'s conversion was deliberately not landed | **CONFIRMED and UPHELD, not completed.** The `INFO` existence check is gone, the blocker and the seven measured severities are recorded at the site (§1c) |
| the same test's `isLoggable` assertion | **A SECOND WRONG PIN, in a test nobody flagged.** HotSpot answers `false` for three levels and NPEs on the argument the test passes; the assertion is relabelled as a divergence witness (§1c) |
| E36-1 N5: `native-io/src/lib.rs:20095` describes registrations that are gone | **CONFIRMED and FIXED**, with the dead `let kinds` binding removed (§2) |
| `vm/src/vm/tests.rs` holds tests that cannot fail | **YES — one with an EMPTY BODY, deleted; one probe with an explicit "don't assert", left and nominated** (§3) |
| tests pinning a registration's EXISTENCE rather than behaviour | **25 tests, 130 `find(...).is_some()`/`is_none()` assertions** (§4a) |
| tests driving a registration a `getstatic` cannot reach | **41 call-sites; 4 deleted here; 36 dead ones remain across 20 tests** — and 7 of them are dead a second time over, because javac inlines the constant and emits no `getstatic` at all (§4b) |
| a value transcribed from the code under test rather than an oracle | **13 sites: `()Ljava/lang/foreign/ValueLayout;` — a descriptor that exists nowhere in JDK 25**, while the same file gets the same field right 3 sites away (§4c) |
| the file's concrete VALUE assertions, spot-checked against HotSpot | **9 of 9 agreed.** The defect in this file is structural, not arithmetic (§4d) |

---

## 1. THE THREE TESTS (E36-1 N2)

### 1a. `watch_event_kinds_p66` — deleted, because no assertion was available

Measured, on the oracle:

```
$ javap -p java.nio.file.StandardWatchEventKinds
  public static final java.nio.file.WatchEvent$Kind<java.nio.file.Path> ENTRY_CREATE;
  public static final java.nio.file.WatchEvent$Kind<java.lang.Object> OVERFLOW;
$ java E40W
  class=java.nio.file.StandardWatchEventKinds$StdWatchEventKind
  name=ENTRY_CREATE
  type=interface java.nio.file.Path
  isString=false     isKind=true     ident=true
  ovf.type=class java.lang.Object
```

The four registrations (`native-builtins/src/phases_late/nio_file.rs`,
`register_p66_watch_service`) each answer `ctx.create_string("ENTRY_CREATE")` —
a `java/lang/String` where the descriptor names a `WatchEvent$Kind`. The test
asserted `read_java_string(...) == "ENTRY_CREATE"`, i.e. it **converted the
defect into a requirement**.

The interesting part is that **the test could not be repaired in place**, and
that is worth stating because "fix the test to assert the JDK's actual type"
sounds like a one-line edit. Every assertion available here is one of:

* **green now, red the moment the type is fixed** — what it did;
* **red now**, because the row answers a String today and the row lives in
  another lane's file;
* **vacuous** — and the test's own second half,
  `assert!(matches!(modify, Value::Object(Some(_))))`, already was: a bare
  String satisfies it, and so does any other non-null.

There is no fourth option while the registration is wrong and unreachable, so
the edit is deletion plus a paired nomination (§5 N1) — not a rewrite. The
comment left in its place carries the measurement, so the next reader does not
have to re-derive it.

**Replacement coverage was added where the behaviour actually lives.** The
consumer is `native-io`'s `watch_event_kind_bit` / `watch_event_kind_object`,
which never touch the native registry — they read the class's STATIC. Nothing
in the tree exercised the translation; the three tests named `test_92_2_watch_*`
in that file drive the `notify` crate, not this VM. Two new tests:

| test | what it would catch |
|---|---|
| `watch_event_kind_bit_translates_all_three_kind_shapes` | all three `Kind` shapes — synthetic (bit in slot 0), REAL (`name()`, quoted off the oracle), legacy bare String — plus the negative: `OVERFLOW` must translate to `0`, not to an entry bit |
| `watch_event_kind_object_round_trips_through_the_bit` | `Path.register`'s encode and `detect_events`' decode agreeing for all three bits — the round trip whose failure once made a watch register successfully and report nothing, forever |

Neither uses `is_some()` or "non-null": the failure mode being guarded is a
*wrongly identified* kind, which is non-null.

### 1b. `string_template_basics_p67` — deleted WHOLE, further than the nomination

```
$ javap -p java.lang.StringTemplate
Error: class not found: java.lang.StringTemplate
```

String templates were preview (JEP 430 in 21, JEP 459 in 22) and were
**withdrawn**. E36-1 N2(b) asked for the `STR` block only, keeping
`of`/`interpolate` because they "exercise real method-shaped registrations".

**Method-shaped is not the same as reachable.** Being an `invokestatic` target
only helps if some classfile can name the class, and on JDK 25 none can — no
javac on this host will compile a reference to it, and
`classloading/src/class_manager.rs` has no stub for it either
(`grep StringTemplate` there returns nothing), so the synthetic side does not
supply one. The whole test drove one withdrawn class, so the whole test goes.

Two facts found while checking, which are why the verdict differs from E36-1's:

* **`fragments` is a second wrong-type native, in the half that was to be
  kept.** It is registered `()Ljava/util/List;` and its body is
  `Ok(Some(ctx.get_field(this, 0)))` — slot 0 is the String handed to
  `of(String)`. It answers a `java/lang/String` where its own descriptor names a
  `java/util/List`: the same defect as §1a's, in the family proposed for
  retention. **No test called it.** So the family was not "half tested, half
  dead"; it was half dead and half untested.
* the deleted test's `interpolate` assertion sat inside an else-less `if let`,
  so an `interpolate` returning `Value::Int(0)` or a null would have passed it.

### 1c. `system_logger_p67` — the decision upheld, and a second wrong pin found

**The `Level` half.** The `INFO` block was an existence check
(`assert!(matches!(info, Value::Object(Some(_))))`) on one of seven field-shaped
rows in `phases_late.rs`. A non-null answer is exactly what a **nameless,
severity-less** enum constant also gives, so the assertion could not distinguish
a working constant from a broken one — the failure mode that zeroed fifteen
netty classes earlier in this session is invisible to it.

E36-1 §4c **decided not to convert** these rows, and this lane upholds that
rather than quietly completing it. Restated at the site so nobody "finishes" it
without reading: `java.lang.System$Logger$Level` is a real JDK class with real
`<clinit>` bytecode in every image; a registered native `<clinit>` beats real
bytecode on the cold interpreter path; and the class carries
`private final int severity`. Measured:

```
$ java E40W
LEVEL ALL ord=0 sev=-2147483648   LEVEL TRACE ord=1 sev=400
LEVEL DEBUG ord=2 sev=500         LEVEL INFO ord=3 sev=800
LEVEL WARNING ord=4 sev=900       LEVEL ERROR ord=5 sev=1000
LEVEL OFF ord=6 sev=2147483647
$ javap -p 'java.lang.System$Logger$Level'   # also: getName(), $VALUES
```

A conversion writing only `name`/`ordinal` shadows the real `<clinit>` and turns
`getSeverity()` into `0` for every level — a dead row traded for a live
regression. And `classloading/src/class_manager.rs` declares **no** statics for
this class (`grep -c` = 0, re-checked today), so a `<clinit>` landed alone would
publish into the void on the synthetic side too. The test now asserts nothing
about `Level`, which is the honest state: a dead row with a recorded blocker is
not coverage.

**The `isLoggable` half — not in any nomination, and worse than the `Level`
half.** The test asserts

```rust
assert_eq!(loggable, Value::Int(1));   // isLoggable(null receiver, null level)
```

Measured on the oracle:

```
$ java E40L
impl=sun.util.logging.internal.LoggingProviderImpl$JULWrapper
isLoggable(ALL)=false    isLoggable(TRACE)=false   isLoggable(DEBUG)=false
isLoggable(INFO)=true    isLoggable(WARNING)=true  isLoggable(ERROR)=true
isLoggable(OFF)=true
isLoggable(null) -> java.lang.NullPointerException: Cannot invoke
  "java.util.logging.Level.intValue()" because "level" is null
```

So HotSpot answers **`false` for the three levels below the default**, and
**throws** for the argument this call actually passes. CratonVM's row is an
unconditional `true` — a fabricated success — and this assertion pinned it as
correct. The fix needs the severity comparison the `Level` blocker above is in
the way of, so the assertion is **kept and relabelled**: same value, same green,
but now carrying the oracle and an explicit "DIVERGENCE PIN, NOT A CORRECTNESS
ASSERTION" so it cannot be read as a correctness statement, and an
`assert_eq!` message telling the next lane not to change it to match a different
fabrication. Nominated as §5 N3.

This is the second time in this record that the *unflagged* half of a test was
worse than the flagged half (§1b's `fragments` was the first).

## 2. `native-io/src/lib.rs:20095` — E36-1 N5, FIXED

What was there:

```rust
    // StandardWatchEventKinds constants. `watch_event_kind_object` prefers the
    // REAL static constant when the class is present, so `event.kind() ==
    // StandardWatchEventKinds.ENTRY_CREATE` … holds …
    let kinds = "java/nio/file/StandardWatchEventKinds";
    r.set_category(__prev_cat);
}
```

A comment describing four registrations, followed by a binding nothing reads and
the end of the function two lines later. It is the mirror of E21-1 §2d's defect:
that one claimed a removal that had not happened, this one claimed a presence
that no longer existed. Both cost a reader the same hour.

The binding is deleted and the comment now says what is true: **this crate
registers no `StandardWatchEventKinds` constants and cannot usefully**; the four
rows the old comment described live in `phases_late/nio_file.rs`, are
field-shaped, dead and type-wrong, with E36-1's verdict DELETE; and the actual
reader is `watch_event_kind_object`, which goes through
`static_field_index_by_name` + `get_static_field`, not the registry — which is
why the identity comparison `event.kind() == StandardWatchEventKinds.ENTRY_CREATE`
does hold in real-JDK mode. The comment names the two new tests, so a future
deletion of them breaks a claim rather than silently orphaning one.

## 3. TESTS THAT CANNOT FAIL

`vm/src/vm/tests.rs` holds **1,525** `#[test]` functions. Ten have no
`assert`/`panic` of any kind. Eight of those ten are honest smoke tests —
`unsafe_fences_no_crash`, `zip_output_stream_p58`, `auto_closeable_close_p70`,
`g67_close_idempotent`, `m4_heavy_allocation_does_not_crash`,
`m4_heap_expansion_under_pressure`, `m4_massive_allocation_with_gc`,
`m7_gc_heap_alloc_array_zero_length` — because their `.unwrap()`s and `.expect()`s
*are* assertions and a panic in the code under test fails them. Weak, not
vacuous. (`zip_output_stream_p58` is the weakest: it builds a ZIP into a
`ByteArrayOutputStream` and never looks at the bytes, so a `ZipOutputStream`
that writes nothing at all passes it. Left, and noted.)

**Two cannot fail under any input.**

* **`multi_catch_exception_handler` (G37) — DELETED.** Its body was **empty**:
  fifteen lines of comment and not one statement. No mutation of the code it
  names can turn it red, because it does not call that code. Its own text admits
  this — *"We can't easily construct bytecode with multi-catch in a unit test,
  but we verify the handler lookup behavior"* — and then substitutes a paragraph
  of code review, *"Verified correct: find_exception_handler at
  interpreter.rs:2374 iterates all entries…"*. That is a claim about a source
  file at a line number, made by a reader; the function has since moved to
  `vm/src/runtime/interpreter/exception_dispatch.rs:226`, so **the citation had
  already rotted and nothing reported it.** Its stated premise is also false
  today: `register_test_class` in this same file takes a `CodeAttribute` with a
  real `exception_table`, and the tests from `reflect_method_invoke_static_void`
  onward drive hand-assembled bytecode through the interpreter. Nominated as
  §5 N4 with the shape the real test must have — including the negative arm,
  without which a `find_exception_handler` that returns the first entry
  unconditionally would pass.
* **`real_jdk_invoke_integer_valueof` — LEFT, deliberately, and nominated.** It
  early-returns when there is no real JDK, then `match`es the result and
  `eprintln!`s in *every* arm, including `Err`, with `// This is expected to
  fail initially — don't assert` at the site. It is a probe, not a test, and it
  has a stated reason. Two lanes in this session correctly declined to
  "complete" a family, and this is the same call: the reason may well be stale
  now, but deciding that requires **running** it, which this lane may not do.
  §5 N5.

## 4. THE SWEEP

### 4a. Existence-of-a-registration, not behaviour — 25 tests, 130 assertions

The shape the task named, and the one a sibling file had 64 of, all 64 of which
stayed green through a real layout defect. Here the count is **130
`find(...).is_some()` plus 12 `.is_none()`**, and **25 tests whose every
assertion is one of them**:

```
copy_on_write_arraylist_basic  timer_basic  simple_date_format_basics_p57
abstract_map_equals_is_deliberately_not_registered_p60
string_joiner_p30_registration_exists  properties_registered
string_modern_methods_registered  phase50_api_completeness (17)
scanner_implementation_registered (11)  phase51_registrations (8)
g68_primitive_pattern_natives_registered  g69_stable_value_natives_registered
g70_module_import_natives_registered  g71_flexible_constructor_natives_registered
g72_implicit_class_natives_registered  g73_byte_vector_natives_registered
g73_short_vector_natives_registered  g73_vector_species_byte_short_registered
g73_int_vector_convert_cast_registered  u6_datagram_channel_natives_registered
g12_biginteger_new_natives_registered (13)
s52_structured_concurrency_natives_registered  s52_config_natives_registered
s52_scoped_value_natives_registered  s52_shutdown_on_failure_inherits_fork_join
```

`phase50_api_completeness` is the clearest: 17 assertions, every one of the form
`registry.find(class, method, descriptor).is_some()`. One of them is

```rust
registry.find("java/util/HashMap", "values", "()Ljava/util/Collection;").is_some()
```

— and `HashMap.values()` returning the wrong *kind* of collection is a defect
this tree has already had and recorded. A registration-exists check cannot see
it. These are **guards checking the tree against itself**: they re-assert a
`register(...)` line that is thirty files away and would be deleted in the same
commit as the row, which is why they have never caught anything.

**Not mass-rewritten, and one of them must not be.**
`abstract_map_equals_is_deliberately_not_registered_p60` is the same *shape*
(`.is_none()`) and is exemplary: it pins a NEGATIVE with a stated mechanism
("an identity shim on an abstract class intercepts every Map subclass that
inherits it") and a bug reference. A negative registration guard is the one case
where existence *is* the behaviour. Nomination §5 N6 asks for the other 24 to be
converted to behavioural calls, per family, not in one sweep.

### 4b. Driving a registration a `getstatic` cannot reach — 41 sites, 36 still live

E21-1 established that no `getstatic` path in this VM consults the native
registry, and E36-1 counted the field-shaped rows by "descriptor does not start
with `(`". **That census cannot see the tests, and it cannot see a row whose
descriptor was fabricated as a method.** Sweeping this file for
`call_native(..., "ALL_CAPS_NAME", ...)` finds **41 sites**. One
(`HttpRequest$Builder.POST`) is a real method. Four are deleted by this lane
(`ENTRY_CREATE`, `ENTRY_MODIFY`, `STR`, `System$Logger$Level.INFO`). **36 remain,
across 20 tests:**

| test | sites | descriptor form |
|---|---|---|
| `level_constants` | 1 | `()Ljava/util/logging/Level;` — fabricated method |
| `locale_us_and_uk` | 2 | field-shaped |
| `standard_charsets` | 1 | field-shaped |
| `selection_key_constants_p58` | 4 | `I` — **doubly dead**, below |
| `spliterator_constants_p59` | 3 | `I` — **doubly dead**, below |
| `http_version_enums_p60`, `http_redirect_enums_p60` | 4 | field-shaped |
| `normalizer_form_enums_p61` | 2 | field-shaped |
| `value_layout_constants_p67` | 3 | field-shaped, **correct JDK descriptors** |
| `number_format_style_enum_p69` | 2 | field-shaped |
| 10 `panama_*` / `struct_layout_*` / `union_layout_*` tests | 13 | `()Ljava/lang/foreign/ValueLayout;` — fabricated, §4c |

**Seven of the 36 are dead a second time over.** `SelectionKey.OP_*` and
`Spliterator.ORDERED/SIZED/DISTINCT` are `static final int` **constant
expressions**, so JLS §13.1 inlining applies and javac emits no `getstatic` at
all. Measured:

```
static int a(); 0: iconst_1      // SelectionKey.OP_READ
static int b(); 0: bipush 16     // Spliterator.ORDERED
```

versus, for the reference-typed ones, a real read that a native still cannot
answer:

```
static Object c(); 0: getstatic java/lang/foreign/ValueLayout.JAVA_INT:Ljava/lang/foreign/ValueLayout$OfInt;
static Object d(); 0: getstatic java/util/Locale.US:Ljava/util/Locale;
static Object e(); 0: getstatic java/nio/charset/StandardCharsets.UTF_8:Ljava/nio/charset/Charset;
static Object f(); 0: getstatic java/util/logging/Level.INFO:Ljava/util/logging/Level;
static Object g(); 0: getstatic java/text/Normalizer$Form.NFC:Ljava/text/Normalizer$Form;
```

Note what the `I` rows are: a test asserting `OP_READ == 1`, `OP_WRITE == 4`,
`OP_CONNECT == 8`, `OP_ACCEPT == 16` — **values that are all correct against the
JDK** — on a registration that no classfile in any mode will ever consult. A
right answer to a question nobody asks. That is why §4d's result (every value
checked was correct) is not reassuring on its own.

### 4c. A value transcribed from the code under test — the 13th and 14th copies of one field

`java.lang.foreign.ValueLayout.JAVA_INT` is declared, on the oracle:

```
public static final java.lang.foreign.ValueLayout$OfInt JAVA_INT;
```

This file names that one field **two different ways**:

* `value_layout_constants_p67` (3 sites) uses
  `Ljava/lang/foreign/ValueLayout$OfByte;` / `$OfInt;` / `$OfLong;` — **the
  descriptors javap prints**;
* thirteen sites in ten `panama_*` / `struct_layout_*` / `union_layout_*` tests
  use `()Ljava/lang/foreign/ValueLayout;` — a **method** descriptor, for a
  field, naming a return type the JDK never uses for it.

`()Ljava/lang/foreign/ValueLayout;` appears nowhere in JDK 25. It cannot have
come from an oracle; it can only have been copied from the registration, which
is the exact failure the task names — *an expected value transcribed from the
code under test*. And the two conventions are 6,000 lines apart in the same
file, so neither ever confronted the other. This is the same "one concept, two
encodings" shape recorded for `jclass` handles, with the twist that here **one
of the two is simply right**, and nothing connects them. Nominated as §5 N7.

### 4d. The value assertions themselves — 9 of 9 agreed with HotSpot

The load-bearing concrete expectations were spot-checked against `java` on
25.0.3+9-LTS rather than read:

```
DF.toPattern=[#0.00]                MF=[Hello, Alice! You are 30 years old.]
loc.toString=[en_US] tag=[en-US]    STE=[com.example.Main.main(Main.java:42)]
URI.scheme=[https]                  HexFormat=[0102ff]
split2=[a, b,c]                     octal=[377]
Object.toString=[java.lang.Object@<hex>]
```

plus `java.util.logging.Level.INFO.intValue()` = 800, `RoundingMode.HALF_EVEN`
ordinal 6, `Month.AUGUST.firstMonthOfQuarter()` = JULY (7),
`ZoneOffset.ofHours(5)` = 18000s / `+05:00`,
`URLEncoder.encode("hello world&foo=bar")` = `hello+world%26foo%3Dbar`,
`ValueLayout.JAVA_INT.byteSize()` = 4. **All agreed.** One further HotSpot
comparison did NOT agree, and it is §1c's `isLoggable`.

The conclusion to draw is narrow: this file's arithmetic is sound, and its
*shapes* are where the rot is. Two of the wrong things found here
(`watch_event_kinds_p66`'s type, `isLoggable`'s unconditional `true`) were found
by asking the oracle a question the test did not ask — not by checking the
number the test did assert.

### 4e. Assertions that a wrong-shaped answer skips — 18 tests

Tests whose **every** assertion sits inside an else-less `if let` / `while let`,
so a native returning the wrong `Value` variant (a null, an `Int` where an
`Object` is expected, `None`) skips the assertion and the test passes:

```
string_to_lower_upper  float_compare_test  collections_reverse_test
service_loader_reads_meta_inf_services  enumeration_interface_p63
hex_format_format_parse_p64  stream_concat_p64  thread_local_random_range_p64
thread_builder_name_start_p66  ssl_engine_p68  panama_sequence_layout_pe2
panama_reinterpret_pe2  struct_layout_single_field
struct_layout_byte_int_alignment  union_layout_takes_max_size
g12_biginteger_bitwise_and_or_xor  g12_biginteger_not
s36_compiled_method_stores_deopt_points
```

(A further ~90 tests use the same `if let` shape but **do** carry
`else { panic!(...) }` and are fine. The 18 above are the ones with no `else`
at any level.)

**NOT FIXED, deliberately.** Adding `else { panic!(…) }` is a strictly
strengthening edit whose outcome is *unpredictable from source*: if a test is
currently passing **because** its pattern never matches, the edit turns it red,
and this lane may not run the suite to find out. That is precisely the case that
must be nominated rather than landed. §5 N8 carries the list and the recipe.

## 5. NOMINATIONS

### N1 — `native-builtins/src/phases_late/nio_file.rs` (NOT this lane's file): delete the four `StandardWatchEventKinds` rows

E36-1's table row 5 already carries verdict DELETE; the only thing blocking it
was `watch_event_kinds_p66`, which is now gone, so the deletion is unblocked and
`call_native`'s panic-on-unregistered no longer bites. Delete the four
`r.register(swek, …)` blocks in `register_p66_watch_service` and the block
comment above them that ends *"Left in place only because `vm/src/vm/tests.rs`'s
`watch_event_kinds_p66` `call_native`s ENTRY_CREATE/ENTRY_MODIFY"* — that
sentence is now false. Behaviour coverage moved to `native-io`'s two new tests.
The bridge ratchet moves by 4.

### N2 — `native-builtins/src/phases_late.rs` (NOT this lane's file): delete `register_p67_string_template` whole

Not just the three `STR`/`RAW`/`FMT` rows E36-1 N2(b) named: the registrar
covers a class that does not exist on JDK 25 (`javap -p
java.lang.StringTemplate` → `Error: class not found`), `class_manager.rs` has no
stub for it, and its `fragments` row answers a `java/lang/String` under a
`()Ljava/util/List;` descriptor (§1b). `string_template_basics_p67` was its only
caller and is deleted. If the registrar is kept for any reason, **`fragments`
must still be fixed or deleted** — it is a live wrong-type native regardless of
what happens to the rest.

### N3 — `native-builtins` (owner of `java/lang/System$Logger.isLoggable`): the unconditional `true`

The row answers `Value::Int(1)` for every level. HotSpot, measured (§1c):
`false` for ALL/TRACE/DEBUG on a default logger, `true` for INFO and above, and
`NullPointerException` on a null level. The correct shape compares the argument
level's `severity` against the logger's, which is the same `severity` field the
§1c `Level` blocker is about — so **N3 and the `Level` conversion are one piece
of work, not two.** Until then the assertion in `system_logger_p67` is labelled
as a divergence pin and must not be "corrected" to some other fabricated value.

### N4 — `vm/src/runtime/interpreter/exception_dispatch.rs` (NOT this lane's file): a real multi-catch test

`multi_catch_exception_handler` is deleted (§3). The replacement belongs in that
module, because `find_exception_handler` is `pub(super)` to
`crate::runtime::interpreter` and is not reachable from `vm/src/vm/tests.rs`.
It must build one `exception_table` with **two entries over the same PC range,
the same `handler_pc`, and two different `catch_type`s**, assert the handler is
found for both, **and assert a third unrelated class is NOT matched** — without
that negative arm, an implementation that returns the first entry
unconditionally passes. If a bytecode-level test is preferred instead,
`register_test_class` in `vm/src/vm/tests.rs` already accepts a `CodeAttribute`
with a populated `exception_table`, which is the premise the deleted test said
did not exist.

### N5 — `vm/src/vm/tests.rs` (THIS lane's file, left undone on purpose): `real_jdk_invoke_integer_valueof`

It cannot fail: every `match` arm only `eprintln!`s, `Err` included, with
`// This is expected to fail initially — don't assert` at the site. The reason
is stated, so it is not an oversight, and deciding whether it is now stale
requires **running** it against a real JDK — which this lane may not do. The
owner who can run it should either promote the `Ok(Some(Value::Object(Some(_))))`
arm to an assertion (if real-JDK mode now serves `Integer.valueOf`) or rename it
to `..._probe` so it stops reading as coverage.

### N6 — `vm/src/vm/tests.rs` (THIS lane's file, left undone on purpose): the 24 existence-only tests

§4a lists them. Converting them is per-family behavioural work — e.g.
`phase50_api_completeness`'s `HashMap.values()` row should call the native and
assert the answer's *class*, not that a row exists — and every conversion can
only be validated by running it. Not landed here for the same reason as N8.
`abstract_map_equals_is_deliberately_not_registered_p60` is **excluded**: a
negative registration guard is the one case where existence is the behaviour,
and it has a stated mechanism and a bug reference.

### N7 — `native-builtins` (owner of the `ValueLayout` rows): one field, two descriptors

Thirteen test sites drive `ValueLayout.JAVA_*` under
`()Ljava/lang/foreign/ValueLayout;`; three drive the same fields under
`Ljava/lang/foreign/ValueLayout$OfInt;` etc. Only the second matches javap
(§4c). Both sets of registrations are unreachable from a `getstatic` either way,
so the resolution is to pick the `<clinit>` shape for the whole family at once —
with `class_manager.rs` declaring the statics first, or the publish is a silent
no-op, which is the state E36-1 §6 recorded for `PosixFilePermission`'s
`$VALUES`.

### N8 — `vm/src/vm/tests.rs` (THIS lane's file, left undone on purpose): the 18 else-less `if let` tests

§4e lists them. The edit is mechanical — add `else { panic!("expected …, got
{v:?}") }` — but **must be landed by someone who can run the suite**, because a
test currently passing *because* its pattern never matches will go red, and that
red is the finding. Suggested order: run the suite once as-is, apply the 18
edits, run again, and treat every new failure as a defect report rather than as
a broken test.

### N9 — `docs/known-issues/jdk-only/INDEX.md` (NOT this lane's file)

```
* `E40-1-the-test-that-pinned-a-wrong-type-and-the-36-sites-a-getstatic-cannot-reach.md`
  — E36-1 N2 and N5 answered: `watch_event_kinds_p66` deleted (it pinned a
  `WatchEvent$Kind` reading back as a String) with behavioural cover added in
  `native-io`, `string_template_basics_p67` deleted whole (the class does not
  exist on JDK 25, and the half that was to be kept holds a second wrong-type
  native), `system_logger_p67`'s dead `Level` check removed and its
  `isLoggable` assertion relabelled as a measured divergence; plus a sweep of
  1,525 tests finding 2 that cannot fail, 25 that pin a registration's
  existence, 36 call-sites a `getstatic` cannot reach (7 of them doubly dead
  through JLS 13.1 inlining), 13 sites carrying a descriptor transcribed from
  the code under test, and 18 whose every assertion an else-less `if let` can
  skip.
```

## 6. Residuals

1. **Nothing here was built, type-checked or run.** `rustfmt` exit 0 on scratch
   copies of both files rules out syntax errors only. The two new `native-io`
   tests have never executed; they rely on `MockNativeContext`'s
   `static_field_index_by_name` answering `None` (the trait default — verified
   in `native-api/src/registry.rs:4303`, the mock does not override it), which
   is what routes `watch_event_kind_object` down the synthetic arm. If a future
   mock gains a statics table, `watch_event_kind_object_round_trips_through_the_bit`
   starts exercising the REAL arm instead — a better test, but a different one,
   and it will need the statics declared or it will silently test nothing.
2. **N1 and N2 are unlanded, so 7 dead rows this lane freed remain registered.**
   Nothing calls them now; the cost is 7 ratchet counts and the reader-hours.
3. **36 unreachable call-sites remain in `vm/src/vm/tests.rs`** (§4b). They are
   not wrong, they are pointed at nothing, and they make 20 tests read as
   coverage of constants the VM cannot actually serve to bytecode.
4. **N6 and N8 are the bulk of the remaining work in this file and are both
   blocked on being able to run the suite.** They are listed rather than
   attempted on purpose: 42 tests edited blind by a lane that cannot execute
   them is how a green suite becomes a red one for reasons nobody can attribute.
5. **`zip_output_stream_p58` still never inspects the bytes it writes** (§3). A
   `ZipOutputStream` whose `putNextEntry`/`finish` write nothing passes it. Not
   changed: asserting on the ZIP bytes needs a run to pin the expected output,
   and the local ZIP header is not something to transcribe from the code under
   test — see §4c for how that ends.
6. **The `notify`-crate tests in `native-io`** (`test_92_2_watch_*`) measure a
   dependency, not this VM. They are legitimate as dependency assumptions but
   should not be counted as WatchService coverage; §1a's two new tests are the
   first coverage the Kind translation has ever had.
