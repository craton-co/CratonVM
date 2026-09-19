# E36-1 — the inverted `name`/`ordinal` fallbacks, the `values()` that returned nine nulls with the difficulty stated as its excuse, and 21 field-shaped rows classified

**2026-08-13, lane E36.** Answers **N3** and the Task-2 half of
`E21-1-getstatic-has-no-native-path.md`. Patches
`native-builtins/src/phases_late/nio_file.rs` and
`native-builtins/src/phases_late.rs`, which this lane owns. The patches are
applied and in the working tree.

**This lane may not build or run the VM, and did not.** Every JDK fact below is
from `javap`/`java` on this host (Microsoft build 25.0.3+9-LTS) and is quoted.
Every claim about CratonVM's behaviour — before and after — is **PREDICTED**
from source. **The eight Rust tests this lane added have never been executed.**
Both files were parse-checked (`rustfmt --edition 2021 --emit stdout` on scratch
copies, exit 0), which rules out syntax errors and nothing else; neither was
type-checked.

---

## 0. Verdict

| claim | verdict |
|---|---|
| E21-1 N3: `posix_file_permission_stub_clinit`'s slot fallbacks are inverted | **CONFIRMED and FIXED** — and the fallbacks were the *smaller* half of the defect (§1) |
| the same file holds another enum-constant minter with hand-written slots | **YES — `p57_alloc_enum`, used by 5 files and ~20 call sites.** Right way round, but by coincidence, and it is where the *other* copy of the two lines lives (§1c) |
| `PosixFilePermission.values()` had nothing to read | **WORSE. It was already registered, and returned NINE NULLS**, with "Can't iterate/capture" written at the site as the reason (§2) |
| `StackWalker$Option`'s 3 rows in `phases_late.rs` are a duplicate-fix shadow | **CONFIRMED against the superseder — DELETED** (§3) |
| the field-shaped family in this lane's two files | **21 rows in 5 clusters.** 7 deleted, 14 classified and left with the paired test change nominated (§4) |
| `File.separatorChar` is "a compile-time platform constant in the real JDK too" (in-tree comment) | **FALSE, measured.** All four `File` separators compile to a real `getstatic` (§4b) |
| E21-1 N4: "no statics table means no `<clinit>` is testable" | **TRUE of `native-api`'s mock, FALSE of `native-builtins`'s.** The `<clinit>` body IS unit-testable and now is (§5) |

---

## 1. THE INVERTED FALLBACKS — and why the fallback pair was the smaller half

### 1a. What was there

```rust
    let ord_idx = ctx.resolve_field_index(P, "ordinal").unwrap_or(0);
    let name_idx = ctx.resolve_field_index(P, "name").unwrap_or(1);
```

`lang_misc` defines `ENUM_NAME_SLOT = 0` / `ENUM_ORDINAL_SLOT = 1`, and
`Enum.name()` reads slot 0. So on a resolution miss this wrote the **ordinal
where `name()` looks** — a nameless enum constant. That constant is non-null,
so every null check passes, while `toString()` answers null, `compareTo` calls
every pair equal, and `Enum.valueOf` matches nothing. One nameless constant in
one JDK enum zeroed **fifteen** netty classes earlier in this session.

**Two defects, not one.** The inversion is the one E21-1 named; the resolution
being scoped to `P` — the RECEIVER class — is the one that decides how often the
inversion fires. `resolve_field_index_by_class_id` returns the MOST-DERIVED
declaration, and an enum may declare its own field called `name`, which shadows
`Enum`'s: Spring Boot's `WebEndpointTest.Infrastructure` does exactly that
(`JERSEY("Jersey")`), which is how `name()` came to answer `"Jersey"` instead of
`"JERSEY"` and took a whole test class down with a
`PreconditionViolationException` before any Spring context started. A
receiver-scoped read does not merely *fall back* wrongly — it can *succeed*
wrongly.

### 1b. What is there now

One resolver, in this file, used by every site:

```rust
pub(crate) const ENUM_NAME_SLOT: usize = 0;
pub(crate) const ENUM_ORDINAL_SLOT: usize = 1;

pub(crate) fn enum_name_ordinal_slots(ctx: &mut dyn NativeContext) -> (usize, usize) {
    let _ = ctx.ensure_class_initialized("java/lang/Enum");
    let name_slot = ctx.resolve_field_index("java/lang/Enum", "name").unwrap_or(ENUM_NAME_SLOT);
    let ordinal_slot = ctx.resolve_field_index("java/lang/Enum", "ordinal").unwrap_or(ENUM_ORDINAL_SLOT);
    (name_slot, ordinal_slot)
}
```

The two constants are a **local copy** of `lang_misc`'s, only because those are
private to that module (`const`, not `pub(crate)`). Promoting them and deleting
this pair is **N4** below. Until then this tree has the same two numbers written
down in **four** places — `lang_misc.rs`, `stack_walker.rs`, `net_channels.rs`
and here — which is exactly the shape that let one of them be inverted without
anything noticing.

### 1c. The sweep for the OTHER enum stub with hand-written slots

`grep '"<clinit>"'` over both files returns exactly one registration, so a
`<clinit>`-shaped sweep finds nothing more. Sweeping by **shape** — a function
that mints an enum constant — finds `p57_alloc_enum`
(`nio_file.rs`, ~30 lines above the `<clinit>`):

```rust
    ctx.set_field(obj, 0, Value::Object(Some(n)));
    ctx.set_field(obj, 1, Value::Int(ordinal));
```

Two bare literals, no resolution at all. **It was the right way round**, which
is precisely why it was dangerous: the two files' two copies of the same pair
disagreed and nothing connected them. It is not a minor site —
`grep -rn p57_alloc_enum` returns **five files** and about twenty call sites
(`http2.rs`'s `HttpClient$Version`/`$Redirect`, `concurrent.rs`'s
`Thread$State`, `text_intl.rs`'s `Normalizer$Form`/`FormatStyle`/
`NumberFormat$Style`, this file's `FileVisitResult`, and `phases_late.rs`'s
`System$Logger$Level`). It now routes through `enum_name_ordinal_slots`, so
there is one order in this file and it is resolved, not asserted.

This is the eleventh case this session of *a correct helper existing while
something else was reached for*, and it arrives with the twist that the copy
which was RIGHT is the one nobody would have looked at.

## 2. THE `values()` THAT WAS ALREADY THERE, AND RETURNED NINE NULLS

E21-1 N3 closes with "same file, separate observation:
`posix_file_permission_stub_clinit` publishes no `$VALUES`, so
`PosixFilePermission.values()` and `EnumSet.allOf` have nothing to read." The
observation is right and the situation is worse than it describes.
`register_p70_file_attributes` **already registered `values()`**, 200 lines
above the `<clinit>`:

```rust
    r.register(pfp, "values", "()[Ljava/nio/file/attribute/PosixFilePermission;", |ctx, _args| {
        let arr = ctx.new_array(ArrayElementType::Reference, 9);
        // Can't iterate/capture — just return the array (elements are null but array exists)
        Ok(Some(Value::Object(Some(arr))))
    });
```

A no-op **with its excuse written at the site**, in the family this session has
already paid for twice. "Elements are null but array exists" is the whole bug
stated as the design: `EnumSet.allOf`, `Class.getEnumConstants` and any
`for (var p : values())` got nine nulls, and the first `p.name()` NPEs. The
stated blocker ("can't iterate/capture") is also false — the registry takes a
`fn` pointer, and every other site in this file that needs a constant list reads
one from a module-level `const`.

It is deleted. The replacement, `posix_file_permission_values`, reads the nine
**statics** and returns a fresh array; `posix_file_permission_value_of` resolves
through the same statics. Both are registered from
`register_posix_file_permission_stub_clinit`, which runs **after** the deleted
row's registrar — so the two rows named the same triple and the fabricated one
was losing only by ordering. That is a duplicate-registration race that nothing
in the tree would have reported.

### 2a. The shape came off `javap`, not off memory

```
$ javap -p java.nio.file.attribute.PosixFilePermission
  OWNER_READ; OWNER_WRITE; OWNER_EXECUTE; GROUP_READ; GROUP_WRITE;
  GROUP_EXECUTE; OTHERS_READ; OTHERS_WRITE; OTHERS_EXECUTE;
  private static final PosixFilePermission[] $VALUES;
  public static PosixFilePermission[] values();   -> getstatic $VALUES; clone(); checkcast
  public static PosixFilePermission valueOf(java.lang.String);
$ java E36L
  pfp fresh=true ident=true n=9 first=OWNER_READ ord8=8 toString=GROUP_READ
  NULL -> java.lang.NullPointerException: Name is null
  BAD  -> java.lang.IllegalArgumentException: No enum constant java.nio.file.attribute.PosixFilePermission.nope
  valueOf identity=true
```

Four measured facts, each encoded and each pinned by a test:

1. **Declaration order is the ordinal** and it is neither alphabetical nor
   sorted by mode bit. It is now one module-level
   `POSIX_FILE_PERMISSION_CONSTANTS`, replacing **three** private copies of the
   same nine strings (the `<clinit>`'s `NAMES`, `posix_permission_bits_from_set`'s
   `NAMES`, and `PFP_NAMES` next to the `rwxrwxrwx` parser). Two of those are
   positionally paired with a `BITS`/`PFP_PAT` table, so a reordering in one
   copy silently changed a permission mask.
2. **`values()` returns a FRESH array** — `values() != values()` while
   `values()[0] == OWNER_READ`, because the real method is `$VALUES.clone()`
   (javap, above). Sharing one array would let a single caller's
   `values()[0] = null` corrupt every later caller.
3. **`valueOf` resolves through the static**, so `valueOf("GROUP_WRITE") ==
   GROUP_WRITE`.
4. Both failure shapes are quoted: `NullPointerException: Name is null` and
   `IllegalArgumentException: No enum constant
   java.nio.file.attribute.PosixFilePermission.nope`. (`PosixFilePermission` is
   top-level, so unlike `HttpClient$Redirect` the `$`→`.` replacement never
   fires here; it is kept for shape-consistency with the sibling helper.)

### 2b. Three more corrections that came with it

* **The `<clinit>` was GC-unsafe.** It allocated the constant, then called
  `create_string`, then wrote the name — holding `obj` live across an
  allocation, the native stale-local family. Now pinned across `create_string`,
  matching `net_channels`'s landed model.
* **`$VALUES` is published**, re-READ out of the statics rather than cached from
  pass one (`new_ref_array` allocates and can move them), which is also what
  makes `values()[i] == CONSTANT`. It currently publishes into the void — see §6.
* **The guard was split from the body.** `is_class_synthetic_stub` stays at the
  registered entry point; `posix_publish_constants` is the body. That is the
  only reason any of this is testable (§5).

## 3. `StackWalker$Option` — DELETED, after checking the superseder

The three rows are gone. Confirmed before deleting, not assumed:

* `stack_walker.rs` registers `("java/lang/StackWalker$Option", "<clinit>",
  "()V")` → `native_option_clinit`, which publishes all three constants **and**
  `$VALUES`/`ENUM$VALUES`, with `name`/`ordinal` resolved against
  `java/lang/Enum`;
* `class_manager.rs` declares the three statics for the stub, so those publishes
  land (unlike §6's case);
* its own unit test `option_clinit_populates_enum_values_array` asserts
  `values()[i]` is `==` the published static *and* that each constant carries a
  populated `name`;
* `grep -rn "StackWalker\$Option"` returns only `class_manager.rs`,
  `stack_walker.rs`, a `jca/cipher.rs` comment and the deleted block — **no test
  called them**, so nothing had to move with the deletion.

The block comment that stood over them already said they were dead and named the
reason they were left: the ratchet. That reason is recorded and superseded here —
`scripts/baselines/jdk-only-bridge-ratchet.json`'s own note says it is already
stale and pending re-freeze, and it moves by three.

**The `jca/cipher.rs` cross-reference is now dangling** ("See `phases_late.rs`'s
three `StackWalker$Option` static-field registrations"). Its diagnosis —
`JceSecurityManager.<clinit>` dying because the `Option` constants read back
null — is unchanged by this deletion, because the constants were never coming
from those rows. Nominated (§7 N3), not edited: that file is another lane's.

## 4. THE FULL SWEEP OF BOTH FILES — 21 field-shaped rows

Instrument: `scratchpad/e36/sweep.py` — every `.register*(…)` whose **second**
string literal (the descriptor slot, since the class is usually a binding) does
not start with `(`. Blind to macro-generated rows, as E21-1's was; neither file
has any.

| # | rows | constant | descriptor | verdict | reason |
|---|---|---|---|---|---|
| 1 | 3 | `java/lang/StackWalker$Option.{RETAIN_CLASS_REFERENCE,SHOW_HIDDEN_FRAMES,SHOW_REFLECT_FRAMES}` | `L…$Option;` | **DELETED** | duplicate-fix shadow: `stack_walker.rs`'s `<clinit>` publishes all three, `class_manager` declares the statics, no test calls them (§3) |
| 2 | 4 | `java/io/File.{separator,separatorChar,pathSeparator,pathSeparatorChar}` | `Ljava/lang/String;` ×2, `C` ×2 | **DELETED** | genuinely read at runtime (§4b) but already published by `vm_util.rs`'s post-clinit `File` fixup, which sets all four by name from the same host values. A second, unreachable publisher. `separatorChar` is already on `jdk-only-dead-everywhere.tsv` as `method-nowhere`. No test |
| 3 | 7 | `java/lang/System$Logger$Level.{ALL,TRACE,DEBUG,INFO,WARNING,ERROR,OFF}` | `L…$Level;` | **CONVERT — nominated, not landed** | reference-typed and measured as a real `getstatic`. Three blockers, none in this lane's files (§4c) |
| 4 | 3 | `java/lang/StringTemplate.{STR,RAW,FMT}` | `L…$Processor;` | **DELETE — nominated, not landed** | nothing to convert: `javap -p java.lang.StringTemplate` on this host answers **`Error: class not found`**. The preview API was withdrawn; the class does not exist on JDK 25. Held only by `string_template_basics_p67` |
| 5 | 4 | `java/nio/file/StandardWatchEventKinds.{ENTRY_CREATE,ENTRY_MODIFY,ENTRY_DELETE,OVERFLOW}` | `Ljava/nio/file/WatchEvent$Kind;` | **DELETE — nominated, not landed** | dead **and type-wrong** (§4d) |

Deleted here: **7**. Classified and left with a paired nomination: **14**.

### 4b. The `File` four, and the comment that was measurably wrong

The comment over `separatorChar` said: *"KEEP: `File.separatorChar` is a
compile-time platform constant in the real JDK too."* That is the one claim in
the block that is measurable, and it is **false**. In the JDK these are
`public static final char separatorChar = fs.getSeparator();` — a method call,
not a constant expression — so JLS §13.1 inlining does **not** apply. Measured:

```
static Object a();  0: getstatic java/io/File.separator:Ljava/lang/String;
static char   b();  0: getstatic java/io/File.separatorChar:C
static Object c();  0: getstatic java/io/File.pathSeparator:Ljava/lang/String;
static char   d();  0: getstatic java/io/File.pathSeparatorChar:C
```

So all four belong in E21-1 §4b's "really does `getstatic`" class, not its
"43 dead twice over" class. E21-1's table split them across two rows and named
only the `char` pair as genuine reads; the `String` pair is the same, and the
comment in this file asserted the opposite about the pair E21-1 got right. They are still
deleted, because being genuinely read is an argument for a `<clinit>`, not for a
registration a `getstatic` cannot see, and `vm_util.rs` already has that
`<clinit>`-equivalent covering all four names.

### 4c. Why `System$Logger$Level` is nominated rather than converted

It is the highest-value cluster in these two files and it is not a cheap
conversion. Three things must land with it:

1. **`class_manager.rs` declares no statics for the class** — `grep
   "System\$Logger\$Level"` returns nothing — and `set_static_field_by_name`
   resolves a DECLARED static and is a silent no-op otherwise. A `<clinit>`
   landed alone publishes into the void. That is not a hypothesis: it is the
   state `HttpClient$Version`'s converted `<clinit>` is in today (E21-1 §3).
2. **It is a REAL JDK class with real `<clinit>` bytecode in every image**, and
   `native_option_clinit`'s own doc records that a registered native beats real
   bytecode on the cold interpreter path — *"whatever this function does not do,
   nothing else does either."* A native `<clinit>` here SHADOWS the JDK's, so it
   must reproduce it whole, including `private final int severity`. Measured on
   the oracle: `ALL=-2147483648, TRACE=400, DEBUG=500, INFO=800, WARNING=900,
   ERROR=1000, OFF=2147483647`. A conversion writing only `name`/`ordinal` turns
   `Level.getSeverity()` into `0` for every level in the mode that matters most —
   trading a dead row for a live regression.
3. `vm/src/vm/tests.rs`'s `system_logger_p67` `call_native`s the `INFO` row.

Landing 1+2 is a separate change with its own evidence. The rows cannot fire
meanwhile, so leaving them costs nothing but the three ratchet counts.

### 4d. The watch-event four are type-wrong, and their test pins the wrong type

Each body is `ctx.create_string("ENTRY_CREATE")` — a `java/lang/String` returned
where the descriptor names a `Ljava/nio/file/WatchEvent$Kind;`. On the oracle the
constant's class is
`java.nio.file.StandardWatchEventKinds$StdWatchEventKind`, its `name()` is
`"ENTRY_CREATE"` and its `type()` is `interface java.nio.file.Path`; a bare
String has none of that. It is the E13-1/E2-1 defect — *a native answering in a
form its descriptor does not name* — and it is invisible only because the rows
are dead.

Nothing depends on them: the real consumer, `native-io`'s
`watch_event_kind_object`, reads the **static** and falls back to a synthetic
one-field `Kind` carrying the bit. Converting belongs with the `WatchService`
work in that crate, which owns the layout.

`vm/src/vm/tests.rs`'s `watch_event_kinds_p66` does not merely keep a dead row
alive — it asserts `read_java_string(create) == "ENTRY_CREATE"`, i.e. it **pins
the wrong type as correct**. A test that freezes VM output locks in the
divergence; this one has been doing so since the rows were written.

### 4e. A "REMOVED" note that is wrong in the other direction

E21-1 §2d warned that a note claiming a removal is not evidence of one. The
mirror case exists too, in `native-io/src/lib.rs`:

```rust
    // StandardWatchEventKinds constants. `watch_event_kind_object` prefers the
    // REAL static constant when the class is present, so `event.kind() ==
    // StandardWatchEventKinds.ENTRY_CREATE` … holds …
    let kinds = "java/nio/file/StandardWatchEventKinds";
    r.set_category(__prev_cat);
}
```

The comment describes registrations; the binding is used by nothing and the
function ends two lines later. **The rows the comment describes are gone and the
comment says they are there.** (`let kinds` is also an unused binding, so this
should be producing an `unused_variables` warning.) Nominated (§7 N5) — that
crate is not this lane's.

## 5. VERIFICATION SCOPE — and a correction to E21-1 N4

### 5a. E21-1 N4 is true of one mock and false of the other

E21-1 N4 says *"every `<clinit>`-shaped native in this repository is therefore
untestable in Rust"*, from `native-api/src/test_mock.rs`, where
`set_static_field` is a no-op and `get_static_field` answers `Int(0)`. That is
correct about **that** mock. It is not correct about
`native-builtins/src/test_utils.rs`'s `MockNativeContext`, which has a real
statics table (`static_fields_override`), a real `static_field_index_by_name`
driven by `set_declared_fields`, and working ref arrays. The `native-builtins`
`<clinit>`s **are** testable there — `stack_walker.rs`'s already is.

The one part of E21-1 N4 that holds for both: `is_class_synthetic_stub` returns
the trait default `false`, so a test aimed at
`posix_file_permission_stub_clinit` measures the guard and reaches no field
write at all. **That is why the body is now a separate function.** The guard
stays at the entry point; the part that can be wrong is the part that is tested.

### 5b. Eight new tests, none of them ever executed

| test | what it would have caught |
|---|---|
| `the_enum_slot_fallbacks_are_name_then_ordinal` | **the reported defect** — asserts the pair is `(0, 1)`, both as constants and through the resolver |
| `the_enum_slots_follow_the_declared_layout_not_a_literal` | the **mutation half**: declares `java/lang/Enum` with its two fields *inverted* and asserts the helper answers `(1, 0)`. An implementation that returns `(0,1)` unconditionally passes the test above and fails this one |
| `an_alloc_enum_constant_carries_a_readable_name` | `p57_alloc_enum` writes a *readable String* in the name slot and an int in the ordinal slot — not "an object exists" |
| `every_posix_permission_constant_is_published_with_its_name` | all nine constants: `name()` **populated and equal to the constant's own name**, ordinal == declaration index, and `$VALUES[i] == ` the static |
| `publishing_twice_keeps_the_first_constants` | the idempotence guard — a second entry must not replace live constants with fresh objects that fail `==` |
| `values_is_a_fresh_array_of_the_interned_constants` | `values() != values()` **and** every element `==` the static — the two halves of the oracle measurement |
| `value_of_returns_the_interned_constant_and_the_jdks_two_failures` | identity through the static, plus both quoted failure messages |
| `the_constant_order_is_the_jdk_declaration_order` | the ordinal order, pinned against a plausible-looking reordering that would also change `posix_permission_bits_from_set`'s masks |

Each declares the statics through `set_declared_fields` before driving the
native. Skipping that step is how a test comes to measure the mock's emptiness
rather than the native: `set_static_field_by_name` is a silent no-op for an
undeclared field on both the mock and the VM, so an assertion made without the
declaration is unfalsifiable.

**No assertion in this set is `is_some()` or "non-null".** A nameless enum
constant is non-null; that is the entire failure mode.

### 5c. Fixture coverage is unchanged and is ZERO

No fixture arm was added. `--synthetic-jdk` is not a flag on this binary
(E21-1 §6c, `run.sh:138-163`), so a gate for the synthetic path is either one
that cannot fail or one that fails every run.

## 6. WHAT THIS LANE COULD NOT DO

`class_manager.rs` declares the nine `PosixFilePermission` constants for the
stub but **not `$VALUES`** — the same omission E21-1 §7 N1 found in
`StackWalker$Option`'s entry. `set_static_field_by_name` resolves a declared
static and is a silent no-op otherwise, so the `$VALUES` publish added in §2b
currently goes nowhere on the synthetic side. It is written now so it starts
working the moment N1 lands, and **`values()` does not depend on it either way**
— it reads the nine statics one by one, so it is correct before and after.

## 7. NOMINATIONS

### N1 — `classloading/src/class_manager.rs` (NOT this lane's file): declare `$VALUES`

*exact literal old text* (≈`:13456`, the end of the `PosixFilePermission` arm):
```
                mk("OTHERS_READ"),
                mk("OTHERS_WRITE"),
                mk("OTHERS_EXECUTE"),
            ]
        }
```
*exact literal new text:*
```
                mk("OTHERS_READ"),
                mk("OTHERS_WRITE"),
                mk("OTHERS_EXECUTE"),
                ClassFileField {
                    access_flags: FieldAccessFlags::PRIVATE
                        | FieldAccessFlags::STATIC
                        | FieldAccessFlags::FINAL,
                    name: cratonvm_types::intern_arc("$VALUES"),
                    descriptor: cratonvm_types::intern_arc(
                        "[Ljava/nio/file/attribute/PosixFilePermission;",
                    ),
                    attributes: vec![],
                },
            ]
        }
```
Without it `posix_publish_constants`'s `$VALUES` write is a silent no-op, and
`EnumSet.allOf` / `Class.getEnumConstants` have nothing to read. The same gap
exists in the `"java/lang/StackWalker$Option"` arm (≈`:15416`) and E21-1 §7 N1
already nominates the `HttpClient` pair; the three should land together, since
they are one omission made three times.

### N2 — `vm/src/vm/tests.rs` (NOT this lane's file): three tests pin dead rows, one pins a wrong type

**These are PAIRS. Each deletion and its test edit must land in the same
change**, or the tree goes red.

**(a) `watch_event_kinds_p66` (≈`:50189`)** — the sharpest of the three, because
it does not merely keep a dead row alive, it asserts the wrong TYPE is right.
Delete the test together with the four `StandardWatchEventKinds` rows in
`native-builtins/src/phases_late/nio_file.rs` (`register_p66_watch_service`).
If the class is to keep coverage, the test worth having drives `native-io`'s
`watch_event_kind_object` and asserts the answer's `name()` — not that a
`WatchEvent$Kind` reads back as the String `"ENTRY_CREATE"`.

**(b) `string_template_basics_p67` (≈`:50712`)** — delete the `STR` block
(the `call_native(st, "STR", "Ljava/lang/StringTemplate$Processor;", &[])` and
its assertion) together with the three `StringTemplate` rows in
`phases_late.rs`. `java.lang.StringTemplate` does not exist on JDK 25; the rest
of the test (`of`/`interpolate`) exercises real method-shaped registrations and
should stay.

**(c) `system_logger_p67` (≈`:50867`)** — the `INFO` block at the end. Do **not**
delete it on its own: it should move to whatever the `System$Logger$Level`
conversion (§4c) lands, and the test then worth having reads `INFO` out of the
static and asserts `name()` is `"INFO"` and `getSeverity()` is `800`.

### N3 — `native-builtins/src/jca/cipher.rs` (NOT this lane's file): a now-dangling cross-reference

≈`:3363` says *"See `phases_late.rs`'s three `StackWalker$Option` static-field
registrations — they cover 3 of the enum's 4 constants (JDK 22 added
`DROP_METHOD_INFO`) and are not consulted for a `getstatic`…"*. Those rows are
deleted. The **diagnosis is unaffected** — the constants never came from them —
but the pointer should read `stack_walker.rs`'s `native_option_clinit`, which is
what does publish them, and where the missing fourth constant would have to be
added. The observation that `Option` has four constants and this VM publishes
three is the useful half and should survive the edit.

### N4 — `native-builtins/src/lang_misc.rs` (NOT this lane's file): promote the two slot constants

```
const ENUM_NAME_SLOT: usize = 0;
const ENUM_ORDINAL_SLOT: usize = 1;
```
→ `pub(crate) const`. Then `stack_walker.rs`, `phases_late/net_channels.rs` and
`phases_late/nio_file.rs` can each delete their local copy and import these.
Four copies of two numbers is what allowed one copy to be inverted for as long
as it was; the fix in this lane removes one copy and makes the remaining
duplication explicit rather than removing it.

### N5 — `native-io/src/lib.rs` (NOT this lane's file): a comment that describes registrations that are gone

≈`:20095-20101`. Delete the unused `let kinds = …` binding and rewrite the
comment to say what is true: the `StandardWatchEventKinds` constants are **not**
registered by this crate, and `watch_event_kind_object` reads the class's static
and falls back to a synthetic one-field `Kind`. As written the comment is the
mirror of E21-1 §2d's — that one claimed a removal that had not happened, this
one claims a presence that no longer exists — and both cost a reader the same
hour.

### N6 — `docs/known-issues/jdk-only/INDEX.md` (NOT this lane's file)

```
* `E36-1-inverted-enum-fallbacks-and-the-field-shaped-rows-that-cannot-fire.md`
  — E21-1 N3 answered: the inverted `name`/`ordinal` fallbacks fixed and routed
  through one resolver, the `PosixFilePermission` enum covered whole
  (`<clinit>`/`values`/`valueOf`, replacing a `values()` that returned nine
  nulls), and 21 field-shaped rows in two files classified — 7 deleted, 14 with
  their paired test change nominated.
```

## 8. Residuals

1. **The eight new tests have never been executed**, and neither file has been
   type-checked. `rustfmt` exit 0 rules out syntax errors only.
2. **N1 is unlanded, so `$VALUES` publishes into the void** on the synthetic
   side. `values()`/`valueOf` do not depend on it.
3. **14 of the 21 field-shaped rows are still registered**, each with its
   verdict and blocker recorded at the site. They cannot fire; the cost is three
   ratchet counts and the reader-hours of finding them again.
4. **The ratchet moves by 7** (`scripts/baselines/jdk-only-bridge-ratchet.json`
   registration counts). It cannot be hand-edited — it is re-taken from a census
   by the gate itself, and this lane may not build.
5. **`posix_file_permission_stub_clinit` is gated on `is_class_synthetic_stub`,
   so in real-JDK mode it publishes nothing** — and it is registered as
   `<clinit>` on a class whose real `<clinit>` it may therefore shadow. That is
   pre-existing and unchanged here; `vm_util.rs`'s post-clinit fixup for
   `PosixFilePermission` exists because of it. Whether the native shadows the
   real `<clinit>` for THIS class was not settled by this lane, and settling it
   is the precondition for §4c's conversion. Note the new `values()` is never
   WORSE than what it replaces under any answer to that question: the deleted
   row returned nine nulls unconditionally, in both modes, whereas this one
   returns nine nulls only if the statics are empty and the real constants
   whenever they are not — including when `vm_util.rs`'s post-clinit fixup is
   what filled them.
6. **`p57_alloc_enum` still mints a fresh instance per call.** Every caller that
   uses it to answer for a *named constant* (rather than to build one during a
   `<clinit>`) hands back objects that fail `==` against the class's own
   statics. The `Thread$State`, `Normalizer$Form`, `FormatStyle` and
   `NumberFormat$Style` call sites in other lanes' files were not audited for
   that here.
