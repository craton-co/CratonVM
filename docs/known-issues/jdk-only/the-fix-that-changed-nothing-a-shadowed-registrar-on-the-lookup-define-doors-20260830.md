# The fix that changed nothing: a shadowed registrar on the `Lookup.define*` doors

**Status: FIXED 2026-08-30.** Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`.

Eleven measured rows on the class-generation surface, one wrong file, and a
34-minute rebuild that produced a byte-identical result.

## 1. What was wrong

`MethodHandles.Lookup`'s three class-defining doors answered
`IllegalArgumentException` to every refusal:

| row | HotSpot 25.0.3+9 | CratonVM (both modes) |
| --- | --- | --- |
| `defineClass(badMagic)` | `ClassFormatError` | `IllegalArgumentException` |
| `defineClass(null)` | `NullPointerException` | `IllegalArgumentException` |
| `defineClass(new byte[0])` | `ClassFormatError` | `IllegalArgumentException` |
| `defineClass(magicOnly)` | `ClassFormatError` | `IllegalArgumentException` |
| `defineHiddenClass(…)` ×4 | same four | same four |
| `defineHiddenClassWithClassData(…)` ×2 | `ClassFormatError` / NPE | `IllegalArgumentException` |
| `defineHiddenClass(b, true, (ClassOption[]) null)` | `NullPointerException` | **no throw** |

**The type is the whole defect.** `ClassFormatError` is an `Error`;
`IllegalArgumentException` is a `RuntimeException`. A bytecode generator guards
its emit with `catch (ClassFormatError)` — because that is what the JVM throws —
so ours sails past the handler and the malformed class escapes to fail somewhere
unrelated. That is the same "reported three frames from the defect" shape the
Groovy cluster cost a day to unwind.

The last row is a different species again: a null varargs array was not a wrong
type but a **missing check**. `parse_nestmate_option` treated null and empty
alike and answered `false`, so a caller passing `(ClassOption[]) null` while
meaning `NESTMATE` got a hidden class in its own nest and no error — surfacing
later as an `IllegalAccessError` from the generated class, nowhere near the call
that dropped the option.

## 2. The first fix changed nothing, and the probe is the only reason we know

Three correct edits went into `classloader.rs::lk_define_class` /
`lk_define_hidden_class`. After a 34-minute release build, all six measured rows
were **byte-identical**.

`--dump-native-registry` said why in one column:

```text
Lookup defineClass    ([B)Ljava/lang/Class;      lookup_define.rs:901  owns_slot=true  inv=3
Lookup defineHiddenClass  ([BZ[…ClassOption;)…   lookup_define.rs:909  owns_slot=true  inv=5
Lookup defineHiddenClassWithClassData …          lookup_define.rs:918  owns_slot=true  inv=0
```

`lookup_define::register_lookup_define_class` re-registers all three triples and
runs **after** `classloader::register_classloader_natives` from *both*
registrars (`lib.rs` and `reflect_annotations.rs`). The classloader copies can
never be dispatched, and the module says so in its own doc comment. The edits
were to dead code.

### This is not a rare trap

One dump, 12 792 registrations: **1 060 shadowed**, of which **568 are
cross-file**.

| shadowed rows | loser | winner |
| ---: | --- | --- |
| 109 | `lang_misc.rs` | `lib.rs` |
| 58 | `native-io/src/lib.rs` | `phases_late/nio_file.rs` |
| 31 | `deprecated_io_util.rs` | `deprecated_util.rs` |
| 28 | `native-collections/src/lib.rs` | `properties_sidetable.rs` |
| 27 | `unsafe_natives_ext.rs` | `lib.rs` |
| 26 | `phases_late/foreign_ffm.rs` | `panama.rs` |

So the prior odds that a native picked by name is the loser are about 4%, and
much higher in the crowded families (net, nio, unsafe, ffm). One command, before
the edit rather than after the build, settles it.

One species IS closed: **zero** rows have a `bridge` losing to a
`synthetic-stub`, so the historical placeholder-shadows-real-implementation bug
cannot recur — `register`'s downgrade rule preserves a chosen kind.

The classloader pair is kept, kept in step with the winner, and now carries a
comment naming `lookup_define.rs` as the owner, so the next person does not buy
the same rebuild.

## 3. Measuring every door first changed the fix twice

**Round one.** Two measured rows on `defineHiddenClass`, five `bad magic` sites
in the source. Probing all of them showed `ClassLoader.defineClass` was
**already correct** at both doors — it raises `LinkageError::ClassFormatError`
and always has. Fixing all five on the strength of two measurements would have
changed two that were right.

**Round two.** Extending the probe from 6 rows to 14 turned up three things the
six could not see:

* `lookupTruncated` / `hiddenTruncated` — correct magic, truncated body. Wrong
  too, and a magic-only fix leaves them wrong. The failure comes back from the
  BACKEND, not from our own check.
* `hiddenNullOptions` — not a wrong type but a missing check (§1).
* `wcdBadMagic` / `wcdNullBytes` — the third door, which no earlier row reached.

## 4. The control row is what kept the fix honest

```text
define.lookupWrongPackage   HotSpot IllegalArgumentException   CratonVM IllegalArgumentException
```

`Lookup.defineClass` has a **split contract**: `ClassFormatError` for bytes that
are not a well-formed ClassFile, `IllegalArgumentException` for bytes naming a
class in a different package from the lookup class. A blanket
IAE→`ClassFormatError` replacement on this surface passes every other row and
breaks this one.

The split falls exactly on `VmError`'s Linkage/Runtime line, so the fix is not a
heuristic:

```text
VmError::Linkage(_) / ::ClassFile(_)  ->  re-throw typed   (ClassFormatError, …)
everything else                       ->  keep the IllegalArgumentException wrapper
```

The backend reports a define into `java.util` as
`RuntimeError::SecurityException` (JVMS §5.3.5, and correct for the
`ClassLoader` door), which is precisely why these doors cannot reuse
`define_class_linkage_error` — it re-types *every* recovered variant and would
answer `SecurityException` where the JDK answers `IllegalArgumentException`.
`lang_system::lookup_define_format_error` is the format-only half.

## 5. The type was never lost — it was stringified and already recoverable

`NativeContext::define_class_full` returns `Result<ClassId, String>`, and the
`String` is `format!("{e:?}")` of the real `VmError`:

```text
Lookup.defineClass: Linkage(ClassFormatError { class_name: "", message: "class file too short (6 bytes; need at least 8 for header)" })
Lookup.defineClass: Runtime(SecurityException { message: "Prohibited package name: java.util.ArrayList …" })
```

`lang_system.rs` already had the recovery — `split_debug_error` +
`typed_define_class_error` — written for the `ClassLoader` door, which is
exactly why that door was already right. The `Lookup` doors simply never called
it. **No trait signature had to change.** The alternative (a typed
`define_class_full`) would have touched the trait, its default impl, the VM
impl and four callers to reach the same place.

## 6. Verification

```text
probes/DynClassGenSweep.java     42 rows, was 22 differing lines / 11 rows, both modes
cargo test -p cratonvm-native-builtins --lib lookup_define    22/22
```

The two unit tests that covered this asserted `is_err()` with a message naming
an exception they never checked — they would have stayed green through the whole
defect. They now assert the variant. Three others passed `Value::Object(None)`
for the varargs argument, i.e. a frame shape no Java call site produces (javac
emits `new ClassOption[0]`); they now pass an empty array, and the null case is
its own test.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out DynClassGenSweep
```
