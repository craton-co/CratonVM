> **RETIRED 2026-08-12 — moved out of `docs/known-issues/jdk-only/`.**
>
> **Its status line — *"the switch that turns it off is NOT WIRED"* — was the single most misleading status in this directory.** All four handback parts are in the tree, verified independently of the reconciliation that first flagged it:
>
> * **A** — `--enable-preview` declared at `vm-cli/src/main.rs:443-444`, applied at `:2940`, with tests `enable_preview_is_a_bare_flag_and_does_not_consume_main_class` (`:7188`) and `enable_preview_defaults_off_like_hotspot` (`:7208`). Commit `de9bedeef`.
> * **B** — `PreviewFeatures.isPreviewEnabled` reads the same bit (`native-builtins/src/lib.rs:14953-14958`, re-export `:4304-4306`); the stale *"We don't parse --enable-preview yet"* comment is gone. Commit `de9bedeef`.
> * **C** — `UnsupportedClassVersionError` with HotSpot's wording: `types/src/error.rs:843`, `vm/src/runtime/exceptions.rs:2362-2363`, `classloading/src/class_manager.rs:5280`. Commit `6ce65f98a`. **C3's open caveat — *"do not invent the placeholder, measure it first"* — was honoured:** the Adoptium 25.0.3.9 measurement is written out at `class_manager.rs:5260-5278` and `<Unknown>` is what it produced.
> * **D** — the two benchmark scripts. This record left D open with a specific command, `od -An -tx1 -N8 <class>`. **It was run on 2026-08-12 against the checked-in artefacts:** `bench-tornado/PolyEvalTornado.class` and `VectorAddTornado.class` both answer `ca fe ba be 00 00 00 45` — minor 0, major 69, **neither preview-stamped**, so neither can reach the new arm. Independently, neither is ever run by CratonVM: `bench-tornado/run.sh:44` execs `tornado`, and the CratonVM arms of `scripts/internal/bench-poly-4way.sh` (`:52`, `:62`, `:72`) run `CpuPolyBench`/`GpuPolyBench`, which nothing compiles with `--enable-preview`.
>
> The reader gate itself is complete and ordered (`reader/src/class_file_version.rs:56`, `:58`, `:62`, order pinned `:319-326`, `rejection_messages_are_hotspots` `:456`), and §3.4's inverted test now asserts the refusal (`reader/tests/vulnerability_fixes.rs:349`). §3.5's refusal to add a `CRATONVM_*` twin is an argued refusal and is held.
>
> **One confirming run is still un-taken** — this record's own Falsifier, which is a confirmation of landed source rather than unfinished work. It is carried forward in `docs/known-issues/jdk-only/README.md` §2.6 with its exact commands.
>
> Previous location: `docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260812.md`.

# CratonVM ran a preview class file HotSpot refuses, and the rule is not "minor == 65535"

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md) — "NOT WIRED" IS
> FALSE. ALL FOUR PARTS OF THE HANDBACK ARE APPLIED.** This was the most
> misleading status line in the directory. W7-31-enable-preview-wiring.md took
> the handback and landed it; verified independently here.
> **A** `--enable-preview` in the CLI — `vm-cli/src/main.rs:443`
> (`#[arg(long = "enable-preview")]`), applied at `:2940`
> (`set_preview_enabled(args.enable_preview)`), test at `:7105`, commit
> `de9bedeef`. **B** `PreviewFeatures.isPreviewEnabled` reads the same bit —
> `native-builtins/src/lib.rs:4266` and `:14709`, same commit.
> **C** `UnsupportedClassVersionError` with HotSpot's wording —
> `types/src/error.rs:843`, arm at `vm/src/runtime/exceptions.rs:2362`, raise
> site `classloading/src/class_manager.rs:5281`, commit `6ce65f98a`.
> **D** covered by the same commits; the reader gate is at
> `reader/src/class_file_version.rs:28`/`:33`, consumed at
> `reader/src/class_reader.rs:132`.
>
> * **Headline: CLOSED in source. Nothing in this record is pending work.**
> * **Cannot adjudicate without a run:** with `Q.class` stamped to `69.65535`,
>   `cratonvm --enable-preview -cp . Q` must print `ran-Q` and
>   `cratonvm -cp . Q` must refuse.

**Status: the refusal is IMPLEMENTED in `reader/src/`, the switch that turns it
off is NOT WIRED and is written out below under
[Out-of-file patch (not applied)](#out-of-file-patch-not-applied).** Nothing was
rebuilt in this lane. Every number below is a measurement of HotSpot Adoptium
25.0.3.9 (`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`) or of the
**pre-change** binary at `C:/craton/CratonVM/target/release/cratonvm.exe` (built
2026-08-11 20:54). No claim is made that the edited source compiles or that the
new refusal fires.

docs/known-issues/jdk-only/W7-18-structured-task-scope-jep505.md found this while
doing something else and recorded it rather than fixing it:

> So CratonVM **accepts a preview class file that HotSpot rejects**, and has no
> flag with which to accept it deliberately. […] it is in the class-file parser,
> not in this file, and it is not what breaks StructuredTaskScope. Recorded, not
> fixed here.

W7-18 also separated three things that are easy to conflate, and **only one of
them is a defect**. That separation is the whole reason this lane is small:

| piece | verdict | why |
|---|---|---|
| the JDK's own preview-API class files are not flagged | **correct, leave alone** | `StructuredTaskScope.class` is `69.0`. Preview-ness of an *API* is a `@PreviewFeature` annotation enforced by **javac**, not a class-file bit. |
| reflective access to a preview API is ungated | **correct, leave alone** | `Class.forName("java.util.concurrent.StructuredTaskScope")` succeeds on plain `java`. By design — it is why W7-18's probe is reflection-only. |
| a **user** class file at `minor == 65535` loads unconditionally | **the defect** | HotSpot refuses it without `--enable-preview`; CratonVM ran it. |

## 1. The oracle

### 1.1 Building the two arms

```sh
JDK="/c/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
cat > P.java <<'EOF'
public class P {
    public static void main(String[] a) throws Exception {
        var s = java.util.concurrent.StructuredTaskScope.<String>open();
        s.close();
        System.out.println("ran-preview");
    }
}
EOF
cat > Q.java <<'EOF'
public class Q { public static void main(String[] a) { System.out.println("ran-plain"); } }
EOF
"$JDK/bin/javac" --release 25 --enable-preview -d . P.java Q.java
```

```
Note: P.java uses preview features of Java SE 25.
```

**`javac --enable-preview` does not stamp every class file it emits. It stamps
the ones that actually use a preview feature.** Measured, and it is the first
thing worth knowing here because the opposite is widely assumed:

```
P.class:  minor version: 65535   major version: 69
Q.class:  minor version: 0       major version: 69
```

Both came out of the same `javac` invocation with the same flags.

### 1.2 Rule 1 — a preview class file needs the flag

```
$ java -cp . P
Error: LinkageError occurred while loading main class P
	java.lang.UnsupportedClassVersionError: Preview features are not enabled for P (class file version 69.65535). Try running with '--enable-preview'

$ java --enable-preview -cp . P
ran-preview

$ java -cp . Q
ran-plain
```

The exact message, which is what the reader now reproduces:

```
Preview features are not enabled for {name} (class file version {major}.{minor}). Try running with '--enable-preview'
```

Note there is **no full stop** after `'--enable-preview'`. That is HotSpot's, not
a transcription slip; it is asserted verbatim in
`reader/src/class_file_version.rs::tests::rejection_messages_are_hotspots`.

### 1.3 Rule 2 — the major version must equal the running JVM's

Hand-edited: bytes 4..5 of a `--release 24` class file set to `FF FF`.

```
$ java -cp . A68p
Error: LinkageError occurred while loading main class A68p
	java.lang.UnsupportedClassVersionError: A68p (class file version 68.65535) was compiled with preview features that are unsupported. This version of the Java Runtime only recognizes preview features for class file version 69.65535

$ java --enable-preview -cp . A68p
Error: LinkageError occurred while loading main class A68p
	java.lang.UnsupportedClassVersionError: A68p (class file version 68.65535) was compiled with preview features that are unsupported. This version of the Java Runtime only recognizes preview features for class file version 69.65535
```

**Byte-identical with and without the flag.** The major-version check runs
*before* the enablement check, so an older release's preview bytecode is never
loadable — a preview feature's encoding is only guaranteed inside the release
that shipped it. The check order is reproduced in `verify`, and the two
`assert_eq!`s that pin it are there specifically so a later refactor cannot
reorder them into "with `--enable-preview`, 68.65535 loads".

```
{name} (class file version {major}.{minor}) was compiled with preview features that are unsupported. This version of the Java Runtime only recognizes preview features for class file version 69.65535
```

### 1.4 The name in the message is the **internal** name

Both messages above used an unpackaged class, where the two forms coincide. A
packaged one separates them. `pk/C.class` at `69.65535` and `pk/D.class` at
`68.65535`, each loaded through `Class.forName` from a plain `69.0` main class:

```
caught: java.lang.UnsupportedClassVersionError: Preview features are not enabled for pk/C (class file version 69.65535). Try running with '--enable-preview'
caught: java.lang.UnsupportedClassVersionError: pk/D (class file version 68.65535) was compiled with preview features that are unsupported. This version of the Java Runtime only recognizes preview features for class file version 69.65535
```

`pk/C`, not `pk.C`. HotSpot builds the message from the parser's `Symbol*`, which
is the slash form. Anything rendering this message must not "helpfully" convert.

### 1.5 The other three messages, so nobody invents them later

`java.lang.UnsupportedClassVersionError` has five distinct texts on this JDK.
The remaining three were measured for the same reason — the reader now emits all
five and a guessed string is worse than none:

| pattern | message |
|---|---|
| `44.0` | `E44 (class file version 44.0) was compiled with an invalid major version` |
| `70.65535` | `D70 has been compiled by a more recent version of the Java Runtime (class file version 70.65535), this version of the Java Runtime only recognizes class file versions up to 69.0` |
| `56.1`, `69.1` | `B56x (class file version 56.1) was compiled with an invalid non-zero minor version` |

Two things fall out. The too-new message names **`69.0`** while the
preview-mismatch message names **`69.65535`** — that asymmetry is HotSpot's.
And `70.65535` reports *too new*, not *preview*: the major bound outranks every
preview rule.

### 1.6 The measurement that decides the shape of the fix

```
$ java -cp . A45p     # 45.65535   ->  ran-A45p
$ java -cp . A52p     # 52.65535   ->  ran-A52p
$ java -cp . B55p     # 55.65535   ->  ran-B55p
$ java -cp . B55x     # 55.1       ->  ran-B55x
$ java -cp . B56p     # 56.65535   ->  UnsupportedClassVersionError (rule 2)
$ java -cp . B56x     # 56.1       ->  UnsupportedClassVersionError (non-zero minor)
```

**Below major 56 the minor version is unconstrained, so `0xFFFF` there is a junk
minor and not a preview marker.** JVMS 4.1 only constrains `minor_version` from
major 56 onwards. The boundary was probed on both sides: 55 accepts, 56 refuses,
with and without the flag.

This is the trap in the obvious implementation. **A check written as
`if minor == 65535 && !preview_enabled { refuse }` refuses `45.65535`,
`52.65535` and `55.65535`, all three of which HotSpot runs.** An over-deny in a
function every loaded class passes through is worse than the under-deny it
replaces — this campaign already carries
docs/known-issues/jdk-only/W6-8-method-invoke-exports-gate.md, whose first open
row is exactly that: `Field.get`/`Field.set` ask the `opens` question
unconditionally and refuse public fields of packages HotSpot allows. The check
therefore keys on
`major >= 56 && minor == 65535`, expressed as an early `Ok` for `major <= 55`,
and `is_preview()` is a method rather than an inline comparison so the next
person to need the predicate cannot get it wrong in a fourth place.

### 1.7 `--enable-preview` also moves `PreviewFeatures.isEnabled`

```
$ java --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED -cp . PF
PreviewFeatures.isEnabled=false
$ java --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED --enable-preview -cp . PF
PreviewFeatures.isEnabled=true
```

This matters because CratonVM **already answers this question**, hardcoded, in
`native-builtins/src/lib.rs` (~line 14662):

```rust
    // We don't parse `--enable-preview` yet (see roadmap), so mirror HotSpot's default: off.
    registry.register_with_kind(
        "jdk/internal/misc/PreviewFeatures", "isPreviewEnabled", "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))), NativeKind::Bridge,
    );
```

Its comment is now half-stale: the reason it says `0` is gone the moment the
flag exists. **A run that loaded a preview class file and then told it preview
was off would be a new divergence created by this fix**, so the two bits must be
set from the same source. That is item (B) of the out-of-file patch.

## 2. CratonVM before the change

Same class files, `cratonvm.exe` built 2026-08-11 20:54:

```
$ cratonvm --enable-preview -cp . A69p
error: unexpected argument '--enable-preview' found

  tip: a similar argument exists: '--enable-native-access'

$ cratonvm -cp . A69p          # 69.65535
ran-A69p
[cratonvm] main-vm run() returned Ok — VM main exiting normally

$ cratonvm -cp . A68p          # 68.65535
[cratonvm] main-vm run() returned Err: Could not find or load main class A68p:
  linkage error: class format error in A68p: unsupported class file version 68.65535

$ cratonvm -cp . B56x          # 56.1
[cratonvm] ... class format error in B56x: unsupported class file version 56.1

$ cratonvm -cp . B55p          # 55.65535
ran-B55p
```

So the accept/refuse divergence is **one row**, not three:

| version | HotSpot, no flag | CratonVM before | CratonVM after (unbuilt) |
|---|---|---|---|
| `69.65535` | refused | **ran** | refused |
| `68.65535` | refused | refused | refused |
| `56.65535` | refused | refused | refused |
| `56.1`, `69.1` | refused | refused | refused |
| `55.65535`, `52.65535`, `45.65535`, `55.1` | ran | ran | ran |
| anything `minor == 0` | ran | ran | ran |

Rules 2 and 3 were **already enforced**, by `ClassFileVersion::is_supported`'s
`self.major == MAX_SUPPORTED.major && self.minor == PREVIEW_MINOR` tail. What
diverged for those rows was never the verdict, only the wording and the
exception type: CratonVM says `class format error in A68p: unsupported class
file version 68.65535` through `java.lang.ClassFormatError`, HotSpot says the
sentence in §1.3 through `java.lang.UnsupportedClassVersionError`.
`UnsupportedClassVersionError extends ClassFormatError`, so a
`catch (ClassFormatError)` in application code already fires — the type is wrong
in the leaf, not in the hierarchy. Closing that is item (C), out of file.

## 3. What changed, and the blast radius

`reader/src/class_file_version.rs`, `reader/src/class_reader_error.rs`,
`reader/src/class_reader.rs`, `reader/src/lib.rs`, and one inverted assertion in
`reader/tests/vulnerability_fixes.rs`.

### 3.1 Exactly which byte patterns take a new path

The version is bytes 4..7 of the file: `minor` big-endian at offset 4, `major`
at offset 6.

**One pattern, and only one, changes verdict:**

```
offset:  0  1  2  3   4  5   6  7
        CA FE BA BE  FF FF  00 45
                     ^^^^^  ^^^^^
                     65535   69
```

`minor == 0xFFFF` **and** `major == 0x0045` (69). Previously loaded
unconditionally; now loads only when preview is enabled, which it is not by
default.

Every other pattern keeps its previous answer bit-for-bit. That is not an
argument from inspection — it is structural. `is_supported()` is now literally
`self.verify(true).is_ok()`, and `verify(true)` is the old function with the
same branches in the same order:

| old `is_supported` branch | `verify(true)` |
|---|---|
| `major < 45 \|\| major > 69` → false | `MajorTooOld` / `MajorTooNew` → `Err` |
| `major <= 55` → true | `Ok` |
| `minor == 0` → true | `Ok` |
| `major == 69 && minor == 0xFFFF` → true | `Ok` |
| otherwise → false | `PreviewMajorMismatch` / `NonZeroMinor` → `Err` |

The only substitution at the call site is `verify(true)` → `verify(preview_enabled())`,
and the two differ on exactly the fourth row. Everything above that row returns
before the `minor == PREVIEW_MINOR` block is reached; everything below it was
already `Err`.

### 3.2 Why the JDK's own class files cannot reach the new arm

Three independent reasons, any one of which is sufficient:

1. **They are `minor == 0`.** Measured in W7-18: `StructuredTaskScope.class` and
   `StructuredTaskScopeImpl.class` are `magic=cafebabe major=69 minor=0
   preview=false` — the most preview-ish classes in `java.base` are not preview
   class files. A preview *API* is a `@PreviewFeature` annotation that javac
   enforces at compile time; it leaves no mark in the class file.
2. `minor == 0` returns `Ok` two branches before any preview arm exists.
3. The check does not key on the API, the module, the loader, the package or the
   major version alone — only on the `minor` field. There is no path by which a
   `minor == 0` file reaches `PreviewNotEnabled`.

### 3.3 Nothing in this tree emits a preview-flagged class file

Checked, because a generated class that tripped the new refusal would break the
boot rather than a workload:

* `classloading/src/proxy_gen.rs:449` and `vm/src/runtime/instrument.rs:2470`
  both write `0u16` for `minor`. Those are the two class-file emitters.
* `vm/build.rs` compiles fixtures marked `// JAVA21+` with
  `javac --release 21 --enable-preview`. On this JDK that combination is a hard
  error — `invalid source release 21 with --enable-preview (preview language
  features are only supported for release 25)` — so the modern pass produces
  nothing at all here; and even where it succeeds, §1.1 shows a file that does
  not use a preview feature comes out `minor == 0`.
* `vm/tests/es_segalloc_arena_dispatch.rs` compiles with `--enable-preview` and
  then **zeroes the minor version bytes on purpose**, with a comment explaining
  that CratonVM rejects a JDK-21-preview-marked file. That normalisation still
  works and is still needed; this change does not touch that row.
* `bench-tornado/run.sh` and `scripts/internal/bench-poly-4way.sh` compile with
  `--enable-preview --release 25` against the TornadoVM jars. Those *would* be
  stamped `69.65535` if the source uses a preview feature. They are hand-run GPU
  benchmarks, not part of any suite, and after this change they need
  `--enable-preview` on the CratonVM side too — which is precisely what the
  out-of-file patch adds. Flagged rather than fixed: neither script is this
  lane's file.

### 3.4 The one test whose assertion was inverted

`reader/tests/vulnerability_fixes.rs::class_file_version_minor_rules_are_enforced_by_reader`
opened with

```rust
    cratonvm_reader::read_class(&preview).expect("current preview class version should parse");
```

which pinned the defect as if it were the rule. It now asserts the refusal, plus
`verify(true).is_ok()` for the accepting half. The accepting half is asserted
through `verify` and **not** by flipping `set_preview_enabled(true)`, because
that switch is a process global shared by every `#[test]` in the binary; a
mutation there would make any future preview-parsing test in the same file
order-dependent, which is the shape
`libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md` records. This is the
only file outside `reader/src/` that was touched, and it is touched because the
change makes it fail, not because the lane wanted it.

### 3.5 The flag surface was deliberately not touched

The seam is a plain Rust function pair in the reader:

```rust
pub fn set_preview_enabled(enabled: bool);
pub fn preview_enabled() -> bool;
```

backed by a `static AtomicBool`, default `false`. **No `CRATONVM_*` variable was
added.** The three guards the pre-push hook runs (`flag_declaration_guard`,
`flag_docs_generated`, `flag_surface`) scan for whole-string `"CRATONVM_[A-Z0-9_]+"`
literals in crate sources; this change introduces none, so
`types/src/flag_groups.rs` and `types/tests/flag-surface.txt` are untouched and
the hook has nothing new to check.

That is a choice, not an oversight. `--enable-preview` is a **JVM argument** with
a spec-mandated spelling that HotSpot, javac and every build tool already agree
on. A `CRATONVM_ENABLE_PREVIEW` twin would be a second way to say the same thing,
would have to be declared, documented and kept in sync with the argument, and
would give a workload a way to turn preview on that HotSpot has no counterpart
for — i.e. a new divergence introduced by the fix for a divergence. The string
`'--enable-preview'` does appear in reader sources, inside HotSpot's message text
and in comments; it is not an env-var literal and not scanned.

A process global rather than a field threaded through `read_class` because that
is the shape of the thing on HotSpot too: `Arguments::enable_preview()` is a
whole-process property fixed before the first class is parsed. Threading it would
have changed the signature of four public entry points and every call site in
`classloading/`, `vm/` and `module.rs` — files this lane does not own.

## Out-of-file patch (not applied)

Four parts. **(A) alone makes the feature usable**; without it the reader refuses
every `69.65535` class file with no way to opt in, which is HotSpot's default
behaviour but not HotSpot's *full* behaviour. (B) is required for correctness the
moment (A) lands. (C) and (D) close the wording and the exception type.

### A. `--enable-preview` in `vm-cli/src/main.rs`

The flag has an exact structural twin four hundred lines away —
`--enable-native-access`, which also sets a process-wide gate immediately after
clap parsing. Copy its shape.

Declaration, beside `enable_native_access` (~line 405):

```rust
    /// Enable preview features (mirrors JDK `--enable-preview`).
    ///
    /// A class file whose `minor_version` is 65535 is a preview class file
    /// (JVMS 4.1) and HotSpot refuses to load it without this flag —
    /// measured: `java.lang.UnsupportedClassVersionError: Preview features
    /// are not enabled for P (class file version 69.65535). Try running with
    /// '--enable-preview'`. CratonVM loaded such a class file unconditionally
    /// until docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md.
    ///
    /// Takes no value, unlike `--enable-native-access`: HotSpot's flag is a
    /// bare boolean.
    #[arg(long = "enable-preview")]
    enable_preview: bool,
```

Apply site, immediately after the `enable_native_access` block (~line 2899), for
the same timing reason its comment already gives — main thread, before
`Vm::new(config)` runs any bytecode, so no class can be parsed against the wrong
value:

```rust
    // --enable-preview: JVMS 4.1 preview class files (`minor_version == 65535`)
    // are refused by the reader unless this is set, matching HotSpot, whose
    // default is also off. Two consumers must see the same bit or the VM would
    // refuse to load a preview class file while telling the class library that
    // preview is enabled:
    //   * cratonvm_reader::set_preview_enabled — the class-file parser's gate;
    //   * jdk/internal/misc/PreviewFeatures.isPreviewEnabled — read by
    //     `Class.isUnnamedClass()` and therefore by JUnit's launcher.
    // Measured on Adoptium 25.0.3.9: `PreviewFeatures.isEnabled` is false
    // without the flag and true with it, so the two really are one bit.
    //
    // Set unconditionally rather than under `if args.enable_preview`, so that
    // an embedder that reuses this path cannot inherit a stale `true`.
    cratonvm_native_builtins::set_preview_enabled(args.enable_preview);
```

**One call, not two, and via `native-builtins` rather than the reader directly —
that is a dependency fact, not a style choice.** `vm-cli/Cargo.toml` depends on
`cratonvm-native-builtins` (line 61) and **not** on `cratonvm-reader`;
`native-builtins/Cargo.toml` depends on `cratonvm-reader` (line 59). So the
re-export in (B) is what makes this line compile. Adding
`cratonvm-reader = { path = "../reader", version = "0.3.0" }` to
`vm-cli/Cargo.toml` and calling the reader directly is the alternative, but it
puts the burden of remembering both consumers on every future caller — which is
the failure (B) exists to prevent.

### B. `PreviewFeatures.isPreviewEnabled` must read the same bit

`native-builtins/src/lib.rs`, ~line 14662. Today:

```rust
    // We don't parse `--enable-preview` yet (see roadmap), so mirror HotSpot's default: off.
    registry.register_with_kind(
        "jdk/internal/misc/PreviewFeatures", "isPreviewEnabled", "()Z",
        |_ctx, _args| Ok(Some(Value::Int(0))), NativeKind::Bridge,
    );
```

Replace the closure body and delete the now-false half of the comment:

```rust
    // `<clinit>` calls this once to cache the `ENABLED` constant. It's reached
    // any time `Class.isUnnamedClass()` is used (e.g. JUnit's launcher during
    // discovery), which otherwise aborts every real-JDK run with
    // UnsatisfiedLinkError before a single test executes.
    //
    // This answers the SAME bit as the class-file parser's preview gate
    // (`cratonvm_reader::preview_enabled`), because on HotSpot it is the same
    // bit: measured on Adoptium 25.0.3.9, `PreviewFeatures.isEnabled` is false
    // on plain `java` and true under `--enable-preview`. Answering 0 while the
    // reader had accepted a `69.65535` class file — or the reverse — would be a
    // divergence manufactured by the fix.
    registry.register_with_kind(
        "jdk/internal/misc/PreviewFeatures",
        "isPreviewEnabled",
        "()Z",
        |_ctx, _args| {
            Ok(Some(Value::Int(i32::from(
                cratonvm_reader::preview_enabled(),
            ))))
        },
        NativeKind::Bridge,
    );
```

plus the re-export the apply site in (A) calls, so `vm-cli` sets one thing rather
than remembering two:

```rust
/// Set preview-feature enablement for the process.
///
/// A thin re-export of the reader's switch. The class-file parser and
/// `jdk/internal/misc/PreviewFeatures` must agree, and a single entry point is
/// how they are kept agreeing — see
/// docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md.
pub fn set_preview_enabled(enabled: bool) {
    cratonvm_reader::set_preview_enabled(enabled);
}
```

**If (B) is skipped, (A) is still an improvement but produces a new, smaller
divergence**: a run with `--enable-preview` would load the class file and then
tell it preview is off.

### C. Throw `UnsupportedClassVersionError` with HotSpot's message

Three pieces, none of them in this lane's files. The reader already computes the
message; it just needs a caller that holds the class name:

```rust
// reader/src/class_reader_error.rs — already landed, quoted here for the caller
pub fn unsupported_class_version_message(&self, internal_name: &str) -> Option<String>
```

**C1.** `types/src/error.rs`, a new `LinkageError` variant beside
`ClassFormatError`:

```rust
    /// JVMS 4.1: the class file's `major.minor` pair is not loadable.
    ///
    /// `message` is HotSpot's, verbatim, from
    /// `ClassReaderError::unsupported_class_version_message` — it already
    /// contains the class name in internal (slash) form, so nothing downstream
    /// should prepend the name again.
    #[error("{message}")]
    UnsupportedClassVersionError { class_name: String, message: String },
```

**C2.** `vm/src/runtime/exceptions.rs`, ~line 1979, beside the `ClassFormatError`
arm:

```rust
        LinkageError::UnsupportedClassVersionError { message, .. } => (
            "java/lang/UnsupportedClassVersionError",
            message.clone(),
        ),
```

Note the arm does **not** do `format!("{class_name}: {message}")` the way the
`ClassFormatError` arm does — HotSpot's message already names the class, in the
middle of the sentence, and prefixing it would produce `P: Preview features are
not enabled for P …`.

**C3.** `classloading/src/class_manager.rs:5252` (`define_class_with_options`).
The `name` parameter there is already the internal slash form — JNI
`DefineClass` passes it through verbatim per spec (`vm/src/native/jni.rs:4195`)
— so it is exactly what HotSpot's message wants, with one caveat noted after the
snippet:

```rust
        let mut class_file = cratonvm_reader::read_class_shared(bytes.clone()).map_err(|e| {
            // A version rejection is `UnsupportedClassVersionError` on HotSpot,
            // not the bare `ClassFormatError` every other reader error maps to.
            // The reader cannot build the message itself: HotSpot's wording
            // embeds the class name, and `this_class` is not read until after
            // the constant pool — long after the version check. `name` is
            // already the internal (slash) form HotSpot prints.
            if let Some(message) = e.unsupported_class_version_message(name) {
                return VmError::Linkage(LinkageError::UnsupportedClassVersionError {
                    class_name: name.to_string(),
                    message,
                });
            }
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: name.to_string(),
                message: e.to_string(),
            })
        })?;
```

**The caveat**: `jni_define_class` passes `""` when the caller supplies no name
(the class manager then derives it from `this_class`). The version check runs
*before* `this_class` is read, so an unnamed `DefineClass` of a preview class
file would produce `Preview features are not enabled for  (class file version
69.65535)` with a doubled space. HotSpot has the same ordering problem and
prints its own placeholder; matching it exactly needs a measurement this lane
did not take. Substituting a literal when `name.is_empty()` is the cheap answer,
but **do not invent the placeholder — measure it first.**

The other three `read_class` sites in that file do not need this:

* `9647` (the upgrade path) maps into `ClassFileError::InvalidClassFile` rather
  than `LinkageError`; the same `if let` goes in front of it if the same
  fidelity is wanted there.
* `4996` is a shape probe that discards the error entirely (`.ok()`), so there
  is nothing to render.
* `7527` is `redefine_class`, which already wraps every parse failure in
  `UnsupportedClassRedefinitionError`. JVMTI redefinition has its own error
  taxonomy; changing it is a separate question.

`vm/src/vm/vm_exec.rs:16017` is a fifth call site and is **being edited by
another lane right now** — left alone deliberately.

Until C lands the refusal still happens, as
`class format error in P: unsupported class file version 69.65535: preview
features are not enabled; try running with '--enable-preview'`, thrown as
`java.lang.ClassFormatError`. `UnsupportedClassVersionError extends
ClassFormatError`, so an application `catch` still fires; only
`e.getClass().getName()` and the text differ.

### D. The two benchmark scripts

`bench-tornado/run.sh:37` and `scripts/internal/bench-poly-4way.sh:85` compile
with `javac -g --enable-preview --release 25`. If the sources use a preview
language feature their class files are `69.65535` and, after this change, the
CratonVM arm of those benchmarks needs `--enable-preview` on the `cratonvm`
invocation too. Neither was run in this lane and neither source was read, so
whether they are actually stamped is **not measured** — check
`od -An -tx1 -N8 <class>` before assuming either way.

## Falsifier

After a rebuild, with the fixtures from §1 (`P.class` at `69.65535`, `Q.class` at
`69.0`, `A68p.class` at `68.65535`, `B55p.class` at `55.65535`):

* `cratonvm -cp . P` must fail where it printed `ran-preview`, and the message
  must contain `Preview features are not enabled for P (class file version
  69.65535)`.
* `cratonvm --enable-preview -cp . P` must print `ran-preview` — and must not be
  an argument-parse error, which is the current answer.
* `cratonvm -cp . Q` and `cratonvm -cp . B55p` must be **unchanged**. `B55p` is
  the over-deny canary; if it starts failing, the check leaked below major 56.
* `cratonvm -cp . A68p` must fail with and without `--enable-preview`, with the
  §1.3 wording. If the flag makes it load, the two checks were reordered.
* **The boot itself is the real test.** Any `--real-jdk` run at all exercises
  ~540 `java.base` classes through the changed branch; if `minor == 0` were
  reachable from the new arm, nothing would start.
* After (C): `e.getClass().getName()` on the caught throwable must be
  `java.lang.UnsupportedClassVersionError`, and the message must not contain the
  class name twice.

## What is not claimed

Nothing was rebuilt. The five HotSpot messages, the 55/56 boundary, the
javac-stamping behaviour, `PreviewFeatures.isEnabled`, and the CratonVM baseline
in §2 are all measurements. They establish that CratonVM ran a class file HotSpot
refuses, what HotSpot says when it refuses, and that exactly one byte pattern's
verdict has to move. They do **not** establish that the edited reader compiles,
that the new refusal fires, or that the out-of-file patch works.
