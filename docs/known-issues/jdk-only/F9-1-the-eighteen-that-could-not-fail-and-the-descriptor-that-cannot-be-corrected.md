# F9-1 — the eighteen tests an `if let` could skip, the two struct layouts HotSpot refuses to build, and the descriptor that cannot be corrected in the test

**2026-08-13, lane F9.** Lands E40-1's N1 and N8, W8-E30-1's NOM-3, and
triages E40-1 §4b/§4c. Patches `native-builtins/src/phases_late/nio_file.rs`,
`regression-suite/run.sh` and `vm/src/vm/tests.rs`, which this lane owns.
**E40-1 N2 is NOT applied and is handed back — see §2.** All patches are in the
working tree.

**This lane may not build or run CratonVM, and did not.** Every JDK fact below
is `javap`/`java` on this host (Microsoft build **25.0.3+9-LTS**) and is quoted.
Every claim about CratonVM's behaviour — before and after — is **PREDICTED**
from source. `vm/src/vm/tests.rs` and `nio_file.rs` were parse-checked
(`rustfmt --edition 2021 --emit stdout` on scratch copies, exit 0), which rules
out syntax errors and nothing else; neither was type-checked. `run.sh` and its
two callers WERE executed — §3 is measured, not predicted.

---

## 0. Verdict

| claim | verdict |
|---|---|
| E40-1 N1: the four `StandardWatchEventKinds` rows are dead and type-wrong | **CONFIRMED and DELETED**, with the oracle quoted at the site (§1) |
| E40-1 N2: delete `register_p67_string_template` | **NOT DONE — `phases_late.rs` is another live lane's file and was being written to 19 s before I reached it.** Exact boundaries handed back (§2) |
| W8-E30-1 NOM-3: `run.sh`'s three hook copies are a pure deletion | **CONFIRMED and DELETED. 1,164 rows compared, 0 differ; mutation control fires 4/4** (§3) |
| E40-1 N8: 18 tests whose every assertion an else-less `if let` can skip | **ALL 18 STRENGTHENED — 25 `else { panic!(…) }` arms added** (§4) |
| how many of the 18 would now go red | **PREDICTED: NONE.** Every one of the 25 patterns matches what its native actually returns; source-traced per arm, table in §4.2. The strengthening is insurance, not a bug report — and two of the arms guard a real skip risk that was never load-bearing before (§4.3) |
| E40-1 §4c: 13 sites carry a descriptor that exists nowhere in JDK 25 | **CONFIRMED, RE-MEASURED AS 14 SITES IN 8 TESTS — AND THE FIX IS NOT AVAILABLE IN THE TEST.** The JDK-true spelling resolves to a DIFFERENT registration with an INCOMPATIBLE object encoding, and the consumer cannot be re-spelled to match because registration order decides that key. Recorded at the site, nominated, not applied (§5) |
| E40-1 §4b: 36 call-sites a `getstatic` cannot reach | **RE-MEASURED: 37 ALL-CAPS sites, 1 a real method, 36 unreachable, across 21 tests.** Triaged into four kinds, two of which are worth different treatment (§6) |
| E40-1 §4d: "the file's arithmetic is sound" | **NOT SOUND EVERYWHERE.** Two tests assert a `MemoryLayout.structLayout` answer HotSpot 25 **refuses to produce at all** — `IllegalArgumentException`. Found by asking the oracle a question the tests did not ask (§5.2) |

---

## 1. E40-1 N1 — the four `StandardWatchEventKinds` rows, DELETED

Measured:

```
$ javap -p java.nio.file.StandardWatchEventKinds        # 25.0.3+9-LTS
  public static final java.nio.file.WatchEvent$Kind<java.lang.Object> OVERFLOW;
  public static final java.nio.file.WatchEvent$Kind<java.nio.file.Path> ENTRY_CREATE;
  public static final java.nio.file.WatchEvent$Kind<java.nio.file.Path> ENTRY_DELETE;
  public static final java.nio.file.WatchEvent$Kind<java.nio.file.Path> ENTRY_MODIFY;
```

The four rows in `register_p66_watch_service` each answered
`ctx.create_string("…")` under `Ljava/nio/file/WatchEvent$Kind;` — a
`java/lang/String` where their own descriptor names a `WatchEvent$Kind`. They
were also unreachable: a `getstatic` in this VM has three implementations and
none consults the native registry.

Both preconditions the nomination named are now met, and I verified them rather
than taking them: `watch_event_kinds_p66` is gone from `vm/src/vm/tests.rs`
(`grep` across the tree finds only the comment marking its deletion), so
`call_native`'s panic-on-unregistered no longer bites.

**Deleted:** the `let swek = …` binding and all four `r.register` blocks. The
registrar is kept — it is still called from `phases_late.rs` — and now carries
the whole history, including the measurement, the three `getstatic` sites, and
an explicit "do not re-add rows here; the conversion belongs in the crate that
owns the WatchService layout". A future author who greps for
`register_p66_watch_service` finds the reason, not an empty function.

**Bridge ratchet moves by 4.** Nothing calls these; behaviour coverage lives in
`native-io`'s `watch_event_kind_bit` / `watch_event_kind_object` tests (E40-1
§1a).

## 2. E40-1 N2 — `register_p67_string_template`, NOT DONE, AND WHY

The nomination is **correct**. I re-measured it:

```
$ javap -p java.lang.StringTemplate
Error: class not found: java.lang.StringTemplate
```

and confirmed E40-1's second finding at the source: `fragments` is registered
`()Ljava/util/List;` and its body is `Ok(Some(ctx.get_field(this, 0)))`, where
slot 0 is the `String` handed to `of(String)` — a second wrong-type native, in
the half E36-1 wanted kept. `string_template_basics_p67` is deleted, so the
registrar has no caller.

**I did not make the edit.** `native-builtins/src/phases_late.rs` is one of the
eight files this session declared OWNED by another live lane, and it is not
stale ownership — it is *active*:

```
$ ls -l --time-style=full-iso native-builtins/src/phases_late.rs
  2026-08-13 07:49:37 ...
$ date
  Thu Aug 13 07:49:56 2026
```

19 seconds. My own line numbers for the function moved by 264 lines between two
greps in this session. An `Edit` is a read-then-write; landing one between
another lane's read and write silently discards their change, and the likelier
outcome — my edit discarded by theirs — would have made this record claim a
deletion that is not in the tree. Neither is worth it for a registrar with no
caller.

**Handed back with exact boundaries** (as of `phases_late.rs` at the time of
writing — re-locate by name, not by number):

* delete `pub(crate) fn register_p67_string_template(r: &mut NativeMethodRegistry)`
  **whole**, `:5641`–`:5736`, together with the four-line banner comment above
  it at `:5636`–`:5639`;
* delete its one call site, `register_p67_string_template(registry);` at `:5566`;
* nothing else references it (`grep -rn register_p67_string_template` returns
  exactly those two lines).

If the registrar is kept for any reason, **`fragments` must still be fixed or
deleted independently** — it is a live wrong-type native regardless.

## 3. W8-E30-1 NOM-3 — `run.sh`'s three hook copies, DELETED and MEASURED

`class_args()`, `class_cp_extra()` and `class_cv_args()` (lines 471–590,
comment included) are deleted. `run.sh` sources `harness-guard.sh` at line 375,
before where they were, so the shared definitions now serve both `run.sh` and
`harness-selfcheck.sh`.

This lane executed `bash`, so §3 is the one part of this record that is not a
prediction.

**Syntax:** `bash -n` clean for all three of `run.sh`, `harness-guard.sh` and
`harness-selfcheck.sh`. `run.sh` still declares none of the three hooks
(`grep -n '^class_args()\|^class_cp_extra()\|^class_cv_args()'` → empty), and
carries **0** CR bytes, so the LF file did not acquire Windows line endings.

**Behaviour — the method W8-E30-1 §3.1 established, reproduced against the
copies I actually deleted.** I extracted the deleted text into a scratch file,
sourced both it and `harness-guard.sh` under renamed function names, and
compared their answers over every hook × every listed class × the four-point
environment matrix the three hooks branch on (`HAVE_MODULE` set/unset ×
`CRATONVM_ARGS` with/without `--jdk-only`):

```
rows compared: 1164   differing: 0
```

1,164 = 3 hooks × 97 names × 4 environments — 58 `CORE_CLASSES` + 38
`JDKONLY_CLASSES` + one deliberately unlisted control (`RNotAListedClass`, to
exercise the `*)` arm). W8-E30-1's own figure was 1,140 against a 95-name list;
the list has grown since.

**The matrix is sensitive, not merely silent.** Negative control: mutate one
arm of the deleted copy (`RTreeRangeGc`'s `--Xmx 64m` → `--Xmx 128m`) and re-run:

```
DIFF env=1 class=RTreeRangeGc hook=class_cv_args shared=[--Xmx 64m] old=[--Xmx 128m]
DIFF env=2 …   DIFF env=3 …   DIFF env=4 …
rows compared: 1164   differing: 4
```

It fires in all four environments. A green 1,164/0 from an instrument that
cannot go red would have been worth nothing.

In place of the deleted block `run.sh` now carries a pointer: where the one
definition lives, that the two were proven equal over the matrix rather than
merely read, and — the part that matters for the next author — **not to re-add
a local copy to override the shared one for a single vector**, since the later
definition wins silently and that divergence is precisely what this migration
removed.

## 4. E40-1 N8 — the 18, STRENGTHENED

### 4.1 What was done

All 18 tests in E40-1 §4e now have `else { panic!(…) }` on every `if let`.
**25 arms** across the 18 (several tests have two or three). Verified
mechanically — for each test, `if let` count == `else` count:

```
string_to_lower_upper 2/2   float_compare_test 2/2   collections_reverse_test 2/2
service_loader_reads_meta_inf_services 1/1   enumeration_interface_p63 1/1
hex_format_format_parse_p64 2/2   stream_concat_p64 2/2
thread_local_random_range_p64 2/2   thread_builder_name_start_p66 1/1
ssl_engine_p68 2/2   panama_sequence_layout_pe2 1/1   panama_reinterpret_pe2 1/1
struct_layout_single_field 1/1   struct_layout_byte_int_alignment 2/2
union_layout_takes_max_size 1/1   g12_biginteger_bitwise_and_or_xor 3/3
g12_biginteger_not 1/1   s36_compiled_method_stores_deopt_points 1/1
```

Each message names the class, method and descriptor and prints the `Value` that
arrived, so a failure reports the divergence rather than "expected object".

### 4.2 Which would now fail — PREDICTED: none

E40-1 declined this edit because the outcome is "unpredictable from source". It
is predictable from source; it just takes reading the callee of all 25 arms.
Every one was traced to the native it calls and to the `Ok(Some(...))` that
native constructs.

| test | the arm's callee | what it returns | verdict |
|---|---|---|---|
| `string_to_lower_upper` | `String.toLowerCase/toUpperCase()Ljava/lang/String;` → `lang_string::native_string_to_{lower,upper}_case` | a `create_string` reference | GREEN |
| `float_compare_test` | `Float.compare(FF)I` → `lang_math::native_float_compare` | `Ok(Some(Value::Int(a.total_cmp(&b) as i32)))`, unconditionally | GREEN |
| `collections_reverse_test` | `ArrayList.get(I)Ljava/lang/Object;` → `native-collections::native_al_get` | the stored element; the three added were `create_java_string` refs | GREEN |
| `service_loader_reads_meta_inf_services` | not a native — `find_bootstrap_class_by_name("java/lang/String")` | see §4.3 | GREEN (with a caveat) |
| `enumeration_interface_p63` | `Collections.emptyEnumeration()` (two registrations, both identical) | `try_alloc_concurrent_synthetic(…)` reference | GREEN |
| `hex_format_format_parse_p64` | `HexFormat.formatHex([B)`, `parseHex(Ljava/lang/String;)[B` | a String ref / a `new_array` ref; both arms of `formatHex`'s null check also return a reference | GREEN |
| `stream_concat_p64` | `Stream.concat` → `p56_stream_concat`; `Stream.toList` → `native_p64_stream_to_list` | both `Ok(Some(Value::Object(Some(_))))` on every path, including `toList`'s empty-list fallback | GREEN |
| `thread_local_random_range_p64` | `ThreadLocalRandom.nextInt(II)I` / `nextDouble()D` | `Value::Int` / `Value::Double`, every path | GREEN |
| `thread_builder_name_start_p66` | `Thread.ofVirtual()Ljava/lang/Thread$Builder;` | a 2-field synthetic reference | GREEN |
| `ssl_engine_p68` | `SSLContext.getDefault()` (three registrations; last-write-wins) | every one allocates and returns a reference. The inner arm is already guarded by an `assert!(matches!(…))` two lines up | GREEN |
| `panama_sequence_layout_pe2` | `MemoryLayout.sequenceLayout` → `pe_sequence_layout` | returns `Object(None)` only when arg 1 is not a reference — it is | GREEN |
| `panama_reinterpret_pe2` | `MemorySegment.reinterpret(J)` → panama's (wins on order) | returns a reference; **would `Err` if native access were denied — but the test module's `LazyLock` calls `set_native_access_enabled(true)`, and an `Err` fails the `.unwrap()` before the pattern anyway** | GREEN |
| `struct_layout_single_field`, `struct_layout_byte_int_alignment`, `union_layout_takes_max_size` | `pe_struct_layout` / `pe_union_layout`; the nested arm reads slot 4 | reference unless the members argument is not an array — it is; slot 4 is set to `offsets_arr` | GREEN *(but see §5.2 — two of these are pinning a fabrication for a different reason)* |
| `g12_biginteger_bitwise_and_or_xor`, `g12_biginteger_not` | `BigInteger.and/or/xor/not` → `bi_alloc_int` | `Ok(Some(Value::Object(Some(…))))`, unconditionally | GREEN |
| `s36_compiled_method_stores_deopt_points` | `cratonvm_jit::ExecutableBuffer::new(64)` | `platform::alloc_executable` → one `VirtualAlloc` of 64 bytes on Windows | GREEN |

**So the finding here is a negative one, and it is worth stating plainly:** the
shape E40-1 flagged is real and 25 assertions really were skippable, but not one
of them was actually being skipped. The strengthening changes no verdict today;
it removes the possibility that a future change to any of those 13 natives turns
a red into a green silently. That is the value, and it is smaller than the
nomination feared — which is itself the answer to "run it and find out".

### 4.3 The two arms that were not merely theoretical

Two of the 25 guard a skip that could really happen, and both are now named at
the site:

* **`service_loader_reads_meta_inf_services`** — its ENTIRE body sat inside
  `if let Some(sid) = string_id`, and the line that populates `string_id` is
  `let _ = cm.load_class("java/lang/String");` — **a discarded `Result`**. If
  that load ever fails, the test builds no JAR, calls no `find_resource`, calls
  no `ServiceLoader`, and reports success. It is predicted green because
  `VmConfig::default()` is `EMBEDDED_DEFAULT_JDK_MODE` = **synthetic**
  (`vm/src/config.rs:225`) and `java/lang/String` is a declared synthetic
  bootstrap class — i.e. green for a reason that has nothing to do with what the
  test is about, which is exactly the fragility the arm now reports.
* **`s36_compiled_method_stores_deopt_points`** — `ExecutableBuffer::new` is one
  W|X mapping. A host that refuses it made this test, whose *only* assertions are
  inside the arm, green. The panic now says so in those words, so the next reader
  does not mistake a hardened host for a passing JIT.

## 5. THE DESCRIPTOR — E40-1 §4c, and why "fix it" is not available in the test

### 5.1 Re-measured, and the fix declined with cause

Measured on the oracle:

```
$ javap -p java.lang.foreign.ValueLayout
  public static final java.lang.foreign.ValueLayout$OfByte JAVA_BYTE;
  public static final java.lang.foreign.ValueLayout$OfInt  JAVA_INT;
  public static final java.lang.foreign.ValueLayout$OfLong JAVA_LONG;
$ javap -c F9Probe        # static Object javaInt() { return ValueLayout.JAVA_INT; }
  0: getstatic  Field java/lang/foreign/ValueLayout.JAVA_INT:Ljava/lang/foreign/ValueLayout$OfInt;
```

`()Ljava/lang/foreign/ValueLayout;` appears nowhere in JDK 25. My census finds
**14 sites in 8 tests** carrying it (E40-1 said 13 in 10; the shape is the same,
the count is re-derived) against **3 sites in 1 test**
(`value_layout_constants_p67`) carrying the `$Of*` spelling `javap` prints.

**The task asked me to fix the descriptor. I did not, and this is the finding
that replaced it.** The two spellings are not two names for one row:

| descriptor | registrar | object |
|---|---|---|
| `()Ljava/lang/foreign/ValueLayout;` | `panama.rs::pe_make_layout` | 3 slots — `[0]=Int(kind)`, `[1]=Int(byteSize)`, `[2]=name` |
| `Ljava/lang/foreign/ValueLayout$OfInt;` | `phases_late/foreign_ffm.rs::p67_layout_object` | 4 slots — `[0]=Long(byteSize)`, `[1]=Long(align)`, `[2]=endian`, `[3]=name` |

Both are registered; the keys differ, so neither shadows the other. Every
consumer these 8 tests then call — `MemoryLayout.structLayout` / `unionLayout` /
`sequenceLayout` — is **panama's**, and panama decodes a member with
`match ctx.get_field(m, 0) { Value::Int(k) => k, _ => 0 }`. An `$Of*` object
carries a `Long` there, so it decodes as `kind == 0`, and `LAYOUT_BYTE == 0`
(`native-api/src/ffi.rs:363`). **Predicted effect of the "one-line fix":**
`sequenceLayout(10, JAVA_INT)` answers 10 instead of 40, `struct{int}` answers
1 instead of 4 — five tests red, for a reason that would read as a regression.

Nor can the consumer be re-spelled to match. `MemoryLayout.structLayout`'s
JDK-true return type `…)Ljava/lang/foreign/StructLayout;` is registered by
**both** files, and `register_pe_panama` runs **after**
`register_p67_foreign_memory` on both paths through `lib.rs`, so last-write-wins
hands that key to panama as well. **There is no self-consistent JDK-true
spelling of these calls.** Which half of each pair a caller gets is decided by
registration order, not by the descriptor it writes. That is a strictly stronger
statement than E40-1 N7's "one field, two descriptors", and it moves the fix
wholly into `native-builtins`.

Recorded where it will be read: a block comment above
`panama_value_layout_constants_pe` carrying the `javap` output, the two object
encodings, the ordering fact and an explicit "the obvious edit would redden every
test below".

### 5.2 What asking the oracle a different question found — two fabricated successes

E40-1 §4d checked the *values* these tests assert and found 9 of 9 correct. I
checked the *calls*, and two of them are calls HotSpot refuses:

```
$ java F9Struct
structLayout(JAVA_BYTE, JAVA_INT)  -> THREW IllegalArgumentException:
    Invalid alignment constraint for member layout: i4
structLayout(JAVA_INT, JAVA_LONG)  -> THREW IllegalArgumentException:
    Invalid alignment constraint for member layout: j8

structLayout(JAVA_BYTE, paddingLayout(3), JAVA_INT) -> byteSize=8  align=4
structLayout(JAVA_INT,  paddingLayout(4), JAVA_LONG) -> byteSize=16 align=8
structLayout(JAVA_INT)                               -> byteSize=4  align=4
unionLayout(JAVA_INT, JAVA_LONG)                     -> byteSize=8  align=8
sequenceLayout(10, JAVA_INT)                         -> byteSize=40
```

**The JDK does not auto-pad a group layout.** The caller writes the padding, and
an under-aligned member is an error. CratonVM's `pe_struct_layout` pads silently
(`offset = ffi::align_up(offset, member_align)`), so:

* `struct_layout_byte_int_alignment` asserts 8/4 for a call the oracle throws on;
* `panama_struct_layout_pe2` asserts 16/8 for a call the oracle throws on.

Their numbers are right *for the padded layout they did not ask for*. Both
assertions are **kept and relabelled** as divergence pins, in the shape E40-1
§1c used for `isLoggable`: same value, same green, with the oracle transcript and
"DIVERGENCE PIN, NOT A CORRECTNESS ASSERTION" at the site and an explicit "do not
'correct' the numbers". Changing them to some other invented answer would be
worse than leaving them; the fix is in `native-builtins` (reject an under-aligned
member), and it is nominated.

`struct_layout_single_field` (4/4), `union_layout_takes_max_size` (8) and
`panama_sequence_layout_pe2` (40) **agree with the oracle** and are untouched
beyond their new `else` arms.

## 6. THE 36 — triaged

Re-measured by parsing every `call_native` whose method slot is an ALL-CAPS
name: **37 sites in 21 tests.** One (`HttpRequest$Builder.POST`,
`(L…BodyPublisher;)L…Builder;`) is a real method. **36 remain**, matching
E40-1's count exactly after its four deletions. They are not one problem:

| kind | sites | tests | verdict |
|---|---|---|---|
| **A. Doubly dead** — `SelectionKey.OP_*`, `Spliterator.ORDERED/SIZED/DISTINCT`, descriptor `I` | 7 | 2 | **Keep, annotate.** Measured: `static int opRead() { return SelectionKey.OP_READ; }` compiles to `0: iconst_1`; `Spliterator.ORDERED` to `0: bipush 16`. JLS 13.1 constant inlining — javac emits **no `getstatic` at all**, so these are unreachable a second time over. Every value asserted is correct. Deleting the tests before the registrations would only lose the record; both are now annotated with the bytecode |
| **B. Fabricated method descriptor for a field** — 14 × `()Ljava/lang/foreign/ValueLayout;`, 1 × `()Ljava/util/logging/Level;` | 15 | 9 | **Blocked in `native-builtins`, §5.1.** Not correctable at the call site |
| **C. JDK-true field descriptor, still unreachable** — `Locale.US/UK`, `StandardCharsets.UTF_8`, `HttpClient$Version.*`, `HttpClient$Redirect.*`, `Normalizer$Form.*`, `NumberFormat$Style.*`, `ValueLayout$Of{Byte,Int,Long}` | 14 | 8 | **Keep.** Measured, these DO emit a real read the registry cannot answer — e.g. `getstatic Field java/nio/charset/StandardCharsets.UTF_8:Ljava/nio/charset/Charset;`. They are the ones a `<clinit>` conversion would actually fix, so they are the right ones to convert first |
| **D. `System$Logger$Level.INFO`** | 0 | 0 | already removed by E40-1 §1c; confirmed absent |

The distinction that matters and that a flat count of 36 hides: **kind C is
convertible and kind A is not.** A `<clinit>` for `Locale`/`Charset`/`Form`
would be consumed by a real `getstatic`; a `<clinit>` for `SelectionKey.OP_READ`
would be consumed by nothing, ever, because javac already folded the constant
into the caller. Any future "convert the field-shaped rows" sweep that treats
the 36 as one population will do 7 units of work with a guaranteed zero return.

## 7. NOMINATIONS

### N1 — `native-builtins/src/phases_late.rs` (NOT this lane's file, and actively being edited): `register_p67_string_template`

E40-1 N2, unchanged and still correct; boundaries and the `grep` that proves it
has no other reference are in §2. Owner: whoever holds `phases_late.rs`. If the
registrar is kept, `fragments` (registered `()Ljava/util/List;`, returns slot 0
which is a `String`) must be fixed or deleted anyway.

### N2 — `native-builtins` (owner of `panama.rs` / `phases_late/foreign_ffm.rs`): the two ValueLayout families

§5.1. Thirteen-plus-one test sites drive `ValueLayout.JAVA_*` under a descriptor
that exists nowhere in JDK 25; three drive the same fields under the spelling
`javap` prints. **Both sets are registered, with incompatible object encodings,
and the group-layout consumers are keyed to only one of them by registration
order.** The resolution is a single family decision — pick one encoding, delete
the other, and only then correct the 14 test sites in one commit with the
consumer. Correcting the test descriptors first turns five green tests red for a
reason that is not the defect. This supersedes E40-1 N7 with the ordering fact.

### N3 — `native-builtins` (owner of `pe_struct_layout`): silent auto-padding

§5.2. `MemoryLayout.structLayout` pads under-aligned members instead of
rejecting them. HotSpot 25 throws
`IllegalArgumentException: Invalid alignment constraint for member layout: i4`.
Two tests pin the fabricated success; both are relabelled as divergence pins and
must not be "corrected" until the native rejects. Note the shape when fixing:
`unionLayout` and `sequenceLayout` **agree** with the oracle, so this is one
function, not the family.

### N4 — `vm/src/vm/tests.rs` (THIS lane's file): `service_loader_reads_meta_inf_services`'s discarded `Result`

§4.3. `let _ = cm.load_class("java/lang/String");` throws away the only signal
that the test's entire body is about to be skipped. The `else` arm added here
converts the skip into a failure, which is the important half; promoting the
`let _` to an `.expect(...)` would report the *cause* instead of the symptom and
is a one-line follow-up for whoever can run the suite.

### N5 — E40-1's own N5 and N6 remain open and are not touched here

`real_jdk_invoke_integer_valueof` (needs a real-JDK run to decide) and the 24
existence-only registration tests (per-family behavioural work). Neither is
blocked on anything this lane did.

### N6 — `docs/known-issues/jdk-only/INDEX.md` (NOT this lane's file)

```
* `F9-1-the-eighteen-that-could-not-fail-and-the-descriptor-that-cannot-be-corrected.md`
  — E40-1 N1 and N8 landed and W8-E30-1 NOM-3 landed: the four dead type-wrong
  `StandardWatchEventKinds` rows deleted, `run.sh`'s three hook copies deleted
  (1,164-row equivalence, mutation control fires 4/4), and all 18 else-less
  `if let` tests strengthened with 25 `else { panic!(…) }` arms — PREDICTED to
  leave all 18 green, each arm traced to its native. Plus: the
  `()Ljava/lang/foreign/ValueLayout;` descriptor CANNOT be corrected in the
  test, because the JDK-true spelling resolves to a second registration with an
  incompatible encoding whose consumer is decided by registration order; and two
  tests assert a `MemoryLayout.structLayout` answer HotSpot 25 refuses to
  produce at all. E40-1 N2 is handed back unapplied — `phases_late.rs` is
  another live lane's file and was written to 19 s before this lane reached it.
```

## 8. Residuals

1. **Nothing Rust here was built, type-checked or run.** `rustfmt` exit 0 on
   scratch copies of `tests.rs` and `nio_file.rs` rules out syntax errors only.
   The 25 new `panic!` messages use inline format captures over `Value` /
   `Option<Value>`, which are `Debug + Copy` (`types/src/value.rs:65`), so they
   should type-check — predicted, not observed.
2. **§4.2's "all 18 green" is the load-bearing prediction of this record.** If
   any goes red, the arm's message names the class, method, descriptor and the
   `Value` that arrived, so the failure is a defect report and not a puzzle. Do
   not weaken the arm — read what it printed.
3. **E40-1 N2 is unlanded**, so `java.lang.StringTemplate`'s registrar and its
   wrong-type `fragments` row are still in the tree with no caller at all.
4. **The 14 kind-B sites and both struct-layout divergence pins stay wrong on
   purpose** (§5). They are annotated at the site; the annotations are the only
   thing standing between the next reader and a five-test regression.
5. **`tests.rs` is CRLF in this worktree and `nio_file.rs` / `run.sh` are LF.**
   Verified after editing: 77,318/77,318 CRLF and 0/0 respectively, i.e. no file
   acquired mixed endings.
6. **The §3 matrix compares the deleted text against the shared text; it does not
   re-verify `run.sh` end to end.** No suite run was in flight and none was
   started — `run.sh` is re-read at a saved byte offset by `bash`, so editing it
   mid-run kills the run.
