# The preview gate had no switch, and HotSpot's nameless-class placeholder is `<Unknown>`

> # A34 2026-08-12 (third pass) — §7's live row REPRODUCES, is WIDER than §7
> # states, and is ALREADY FIXED IN SOURCE. See §8. Do not write the patch.
>
> Three things, in the order a reader needs them.
>
> 1. **All three falsifiers were re-run on a fresh binary and §7's verdict
>    holds.** `--enable-preview` parses and gates; `defineClass` on a 69.65535
>    class file still throws `ClassFormatError` wrapping a Rust `Debug` string
>    where HotSpot throws `UnsupportedClassVersionError`.
> 2. **§7 measured the defect through too narrow an aperture.** It probed only
>    `defineClass(null, …)` and filed the finding under the `<Unknown>`
>    falsifier, which frames it as a nameless-define problem. It is not.
>    `defineClass("P", …)` — an ordinary NAMED define — flattens identically.
>    The defect is the `defineClass` road, not the nameless case on it, and any
>    fix scoped to nameless defines would have closed the wrong half.
> 3. **The fix landed in `67146db71` (2026-08-12 17:49) and this record has not
>    caught up.** `native-builtins/src/lang_system.rs` grew
>    `define_class_linkage_error` / `typed_define_class_error`, which parse the
>    backend's `Debug` rendering back into the typed `LinkageError` and are
>    wired into `defineClass0`, `defineClass1` **and** `defineClass2` — the
>    three roads §7 names — with unit tests beside them. §7's own prescription
>    (*"give `define_class_format_error` a pass-through for an already-typed
>    `VmError::Linkage`"*) is superseded by a recovery that also handles the
>    already-flattened case. **Anyone acting on §7 would be re-writing landed
>    work**, which is the failure mode this campaign carries fifteen instances
>    of.
>
> The reproduction below is therefore a statement about BINARIES, not about the
> tree: no binary available to this lane carries `67146db71`. Verified rather
> than assumed — neither `scratchpad/bin/cratonvm-merged-dev.exe` (15:27) nor
> `C:/craton/synjdk-target/release/cratonvm.exe` (17:57) contains the symbol
> `typed_define_class_error`, and both reproduce the old shape identically in
> `--jdk-only` and `--real-jdk`.

> # NOT RETIRED 2026-08-12 (second pass). The build arrived, the falsifiers were
> # run, and the third one found a LIVE row. See §7.
>
> The headline is discharged by measurement: `--enable-preview` parses, the gate
> refuses without it and admits with it, and `PreviewFeatures.isEnabled` agrees
> with HotSpot on **both** arms — which is the falsifier this record itself says
> one arm cannot satisfy. §6's WILL NOT FIX on the `<Unknown>` vs `""`
> distinction stands and was not re-opened.
>
> **What is live is part C on the road part C was written for.** A nameless
> define of a 69.65535 class file renders `<Unknown>` correctly and then throws
> the WRONG TYPE: `java.lang.ClassFormatError` wrapping a Rust `Debug` string,
> where HotSpot throws `UnsupportedClassVersionError`. Measured identically on
> `--jdk-only`, `--real-jdk` and the pristine-dev control, so it is pre-existing
> rather than this wave's. Full measurement and the site in §7;
> RETIREMENT-20260812B.md §3.1 records why this held the record back.

**Status: CLOSED 2026-08-12. All four parts of W7-28's handback are APPLIED, and
the one residual — the `<Unknown>` vs `""` nameless-define distinction — is
ADJUDICATED WILL NOT FIX in §6, which is this record's own §3.1 argument
carried to a decision. Nothing was rebuilt; what this record still needs is a
build, not a patch.**
Every measurement below is of HotSpot Adoptium 25.0.3.9
(`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`) or of the
**pre-change** binary at `C:/craton/CratonVM/target/release/cratonvm.exe` (built
2026-08-11 20:54, which predates the reader change as well as this one). No
claim is made that the edited source compiles or that the new flag works.

docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md landed the refusal
in `reader/src/` and wrote out four parts it did not own. This record applies
them and closes the one caveat it left open by measurement.

## 1. Each part was still needed — verified against the source, not the record

This campaign carries fifteen records claiming a patch was never applied when it
already was, so each site was read before it was touched. **All four were
genuinely unapplied**, and the record's structural claims all held:

| claim | verified |
|---|---|
| `set_preview_enabled` exists in `reader/src/` and **nothing calls it** | yes — the only hit outside `reader/src/` was a *comment* in `reader/tests/vulnerability_fixes.rs:364` explaining why that test deliberately does not call it |
| `--enable-preview` is an argument-parse error | yes, run directly (§2) |
| `isPreviewEnabled` is hardcoded to `Value::Int(0)` | yes, `native-builtins/src/lib.rs:14667` |
| no `LinkageError::UnsupportedClassVersionError` variant | yes, absent from `types/src/error.rs` |
| `class_manager.rs:5252` maps every reader error to `ClassFormatError` | yes |
| **`vm-cli/Cargo.toml` does not depend on `cratonvm-reader`** | **yes** — it lists `cratonvm-vm`, `-jit`, `-classloading`, `-native-api`, `-types`, `-native-builtins`, `-jfr`, and no reader. `native-builtins/Cargo.toml:59` does. The re-export is load-bearing exactly as claimed. |

One disagreement, and it is small: W7-28 §D asserts the two benchmark scripts
"need `--enable-preview` on the CratonVM side too — which is precisely what the
out-of-file patch adds". Measured, they do not. See §5.

## 2. The divergence, reproduced directly

A plain `69.0` class file with bytes 4..5 overwritten to `FF FF` — no preview
API, no `StructuredTaskScope`, so nothing but the version field is in play:

```
$ od -An -tx1 -N8 Q.class
 ca fe ba be ff ff 00 45

$ java -cp . Q
Error: LinkageError occurred while loading main class Q
	java.lang.UnsupportedClassVersionError: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'

$ java --enable-preview -cp . Q
ran-Q

$ cratonvm.exe -cp . Q                    # pre-change binary
ran-Q
[cratonvm] main-vm run() returned Ok — VM main exiting normally

$ cratonvm.exe --enable-preview -cp . Q   # pre-change binary
error: unexpected argument '--enable-preview' found
  tip: a similar argument exists: '--enable-native-access'
```

That is the whole lane: HotSpot has two answers, CratonVM had one, and the flag
that would have selected the other did not parse.

## 3. The caveat W7-28 left open: HotSpot prints `<Unknown>`

W7-28 §C3 flagged that `jni_define_class` passes `""` for a nameless define, that
the version check runs before `this_class` is read, and that **HotSpot's
placeholder was not measured — do not invent it.** Measured.

The reachable analogue of JNI `DefineClass` with a NULL name is
`ClassLoader.defineClass(null, b, 0, b.length)`; both reach the same VM entry
point. Loading the 69.65535 `P.class` five ways from a plain `69.0` main class,
on plain `java`:

```
ClassLoader.defineClass(name=null) -> java.lang.UnsupportedClassVersionError: Preview features are not enabled for <Unknown> (class file version 69.65535). Try running with '--enable-preview'
ClassLoader.defineClass(name="")   -> java.lang.UnsupportedClassVersionError: Preview features are not enabled for  (class file version 69.65535). Try running with '--enable-preview'
ClassLoader.defineClass(name="P")  -> java.lang.UnsupportedClassVersionError: Preview features are not enabled for P (class file version 69.65535). Try running with '--enable-preview'
Lookup.defineClass                 -> ... for P ...
Lookup.defineHiddenClass           -> ... for P ...
```

**`<Unknown>`.** Not blank, not `null`, not the doubled space W7-28 predicted —
that prediction was right about the *shape* and wrong about the *cause*: the
doubled space is what HotSpot prints for an explicitly empty name, which is a
different input from no name at all.

`<Unknown>` is **generic, not preview-specific**. The same nameless define at the
other four version rejections, with and without `--enable-preview` (identical
both ways):

```
68.65535 -> <Unknown> (class file version 68.65535) was compiled with preview features that are unsupported. This version of the Java Runtime only recognizes preview features for class file version 69.65535
70.65535 -> <Unknown> has been compiled by a more recent version of the Java Runtime (class file version 70.65535), this version of the Java Runtime only recognizes class file versions up to 69.0
69.1     -> <Unknown> (class file version 69.1) was compiled with an invalid non-zero minor version
44.0     -> <Unknown> (class file version 44.0) was compiled with an invalid major version
```

So the substitution goes in the caller, once, for all five messages — which is
what `class_manager.rs` now does.

### 3.1 The one knowing inaccuracy, stated rather than hidden

**HotSpot distinguishes a null name from an explicitly empty one. CratonVM
cannot.** `native-builtins/src/classloader.rs:3791`
(`read_optional_internal_name`) folds Java `null` and Java `""` into the same
Rust `String::new()`:

```rust
    match args.get(idx) {
        Some(Value::Object(Some(o))) => { ... }
        _ => String::new(),
    }
```

and `vm/src/native/jni.rs:4192` uses `""` as its own placeholder for JNI's NULL.
By the time `define_class_with_options` sees the name, the two inputs are one
value. Substituting `<Unknown>` therefore makes the reachable case (JNI NULL,
`ClassLoader.defineClass(null, ..)`) exactly right and gives the pathological
case (`defineClass("", ..)`, a caller passing a deliberate empty name) null's
message instead of HotSpot's doubled space.

That is the right trade and it is not free: **it is recorded here rather than
fixed** because fixing it means threading an `Option<&str>` through
`read_optional_internal_name`, `jni_define_class` and `define_class_with_options`
— three files, two of which this lane does not own — to preserve a distinction
whose only observable consequence is which of two placeholder spellings appears
in a message for a class nobody named. Should someone want it, the shape is
`Option<&str>`, not a second sentinel string.

## 4. What was applied

### (A) `vm-cli/src/main.rs`

Declaration beside `enable_native_access`, apply site immediately after its
block. A bare `bool`, not `Option<String>` — HotSpot's flag takes no value.

The one thing worth checking that W7-28 did not: **a bare boolean needs no
`VALUE_TAKING_OPTS` entry.** That table (`vm-cli/src/main.rs:1045`) exists so
the separator inserter does not mistake an option's value token for the main
class name; its own doc comment says "Boolean flags also need no entry", and
`--enable-native-access` is absent from it too. Verified by reading the four
pre-clap stages: an unrecognised `--`-prefixed token in the leading section is
copied through verbatim (`main.rs:1271-1274`). Two tests pin this — one runs the
whole pipeline and asserts `Main` survives as the class name, one asserts the
flag defaults off.

Set **unconditionally** (`set_preview_enabled(args.enable_preview)`) rather than
under an `if`, so an embedder reusing this path cannot inherit a stale `true` —
which is the one way this differs from `--enable-native-access` above it, whose
gate is only ever opened.

### (B) `native-builtins/src/lib.rs`

The closure now reads `cratonvm_reader::preview_enabled()`, and the half of the
comment that said "we don't parse `--enable-preview` yet" is gone because it is
no longer true. `NativeKind::Bridge` is unchanged — it was always a Bridge, and
is arguably more of one now that it reports a real bit rather than a constant.
No effect on `native-builtins/tests/stub_ratchet.rs`, which censuses
`SyntheticStub` registrations only.

The `set_preview_enabled` re-export is placed beside `real_jca_mode` at the
crate's other top-level gate accessors.

### (C) `UnsupportedClassVersionError`

Three files. The arm in `vm/src/runtime/exceptions.rs` does **not** prefix the
class name, per W7-28: HotSpot's sentence already contains it, mid-sentence, and
prefixing would render `Q: Preview features are not enabled for Q ...`.

**Exhaustiveness.** A swept census of every `match` over `LinkageError` in the
workspace found **exactly one exhaustive site**, and this lane owns it:

* `vm/src/runtime/exceptions.rs:1910` (`fn linkage_throwable`) — all ten variants
  listed, no catch-all, match is the function's tail expression. **Edited.**

Every other site carries `other => other` or `other => panic!(..)` and compiles
unchanged: `classloading/src/verify_insn.rs:1369`;
`classloading/src/verifier.rs:739, 948, 1129, 1275`;
`classloading/src/bytecode_verifier.rs:548, 787, 897, 1052`;
`vm/src/runtime/resolve/mod.rs:350` (`impl From<LinkageError> for ResolveError`,
five variants then `other => ResolveError::Internal`). The nested
`VmError::Linkage(..)` matches in `vm/src/runtime/exceptions.rs:~2060`,
`native-builtins/src/{lang_system.rs:5564, lib.rs:4726, lang_class.rs:2963}` and
the verifier tests are all catch-alled too. The `if let` / `matches!` sites never
break. There is no `use LinkageError::*` anywhere, so no bare-variant arm is
hiding from the grep. **Nothing was out of reach.**

Note what the new variant does *not* change: `ResolveError::Internal`'s catch-all
now swallows an `UnsupportedClassVersionError` reaching resolution into a generic
internal error. That was already true of `ClassFormatError` and is out of scope.

**One residual risk, unmeasured.** `linkage_throwable` returns a class name that
`create_exception_object` must materialize. `java/lang/UnsupportedClassVersionError`
is a real `java.base` class, so `--real-jdk` and `--jdk-only` are fine; under
`--features synthetic-jdk` it is no more registered than `java/lang/ClassFormatError`
is, so both are in the same position. If it cannot be built, `throw_linkage_error`
already degrades with a `tracing::warn!` rather than crashing
(`vm/src/runtime/exceptions.rs:2028-2038`).

### (D) The two benchmark scripts do **not** need the flag

W7-28 said they would. Measured, they do not, for two independent reasons either
of which is sufficient.

**Both class files are `minor == 0`.** They are checked into the tree, so this is
a direct read, not an inference:

```
$ od -An -tx1 -N8 bench-tornado/VectorAddTornado.class
 ca fe ba be 00 00 00 45
$ od -An -tx1 -N8 bench-tornado/PolyEvalTornado.class
 ca fe ba be 00 00 00 45
```

`00 00` minor, `00 45` major — plain 69.0. This is W7-28 §1.1's own finding
turned on the actual artifacts: `javac --enable-preview` stamps only files that
*use* a preview feature, and neither of these does. `--enable-preview` is on
those `javac` lines for TornadoVM's benefit, not because the sources need it.

**Neither compiled class is ever run by CratonVM.** `bench-tornado/run.sh:43`
ends `exec tornado ... VectorAddTornado` — there is no `cratonvm` invocation in
that script at all. In `scripts/internal/bench-poly-4way.sh` the three CratonVM
arms (lines 53, 62, 71) all run `CpuPolyBench`/`GpuPolyBench` out of
`$BENCH_CLASSES` (`apps/gpu-bench/classes`, built elsewhere and not with
`--enable-preview`); the `javac --enable-preview` at line 85 compiles
`PolyEvalTornado` for arm 5, which runs under the `tornado` launcher.

So: no edit needed, and none made. Recorded rather than guessed, as asked. If a
future revision of either source adopts a preview language feature, the first
reason lapses and only the second holds — and the second holds for the tornado
arm regardless, because that arm is not CratonVM.

## 5. Hazards checked

* **`Compatible` stays byte-for-byte for non-preview class files.** Unchanged by
  this lane — the verdict logic is entirely in `reader/`, which this lane did not
  touch. What changed here is the *wording and throwable type* of rejections that
  already happened, plus the flag that can now turn the gate off.
* **The over-deny canary.** `45.65535`, `52.65535`, `55.65535` must keep running;
  no check was added anywhere near them. `class_manager.rs`'s new branch fires
  only when the reader already returned `UnsupportedVersion`, so it cannot cause
  a refusal — it can only re-word one.
* **Flag surface untouched.** `git diff dev | grep -o 'CRATONVM_[A-Z0-9_]*'` over
  the added lines returns nothing. No `types/src/flag_groups.rs`,
  `types/tests/flag-surface.txt`, `docs/flag-tokens.md` or
  `docs/config/flag-inventory.md` change is needed, and none was made. This is
  W7-28 §3.5's deliberate choice carried forward: `--enable-preview` is a JVM
  argument with a spec-mandated spelling, and a `CRATONVM_*` twin would give a
  workload a way to enable preview that HotSpot has no counterpart for.
* **`native-builtins/tests/stub_ratchet.rs`** is owned by another lane and was
  not touched. The registration's `NativeKind` is unchanged, so its census is
  unaffected.

## Out-of-file patch (not applied)

**The index row.** `docs/known-issues/jdk-only/README.md` is not this lane's
file. Its header currently reads "**22 records**"; W7-28 and this record are both
new since that count was written, so whoever owns it should reconcile the number
along with adding rows for both.

**The nameless-name distinction**, if anyone ever wants HotSpot's doubled space
for `defineClass("")`: change `read_optional_internal_name` to return
`Option<String>` (`native-builtins/src/classloader.rs:3791`), thread it through
`jni_define_class` (`vm/src/native/jni.rs:4192`, which already has its own `""`
placeholder and a comment saying so) and `define_class_with_options`, then render
`None` as `<Unknown>` and `Some("")` as `""`. §3.1 argues this is not worth it.

## 6. ADJUDICATED 2026-08-12 — the nameless-define distinction is WILL NOT FIX, and this record is closed

§3.1 argued against it; a later lane was asked to decide rather than implement
reflexively, and the decision is to **decline it and close the record**. A record
left open on the one residual its own author argued against is bookkeeping debt,
and it was the only thing keeping this one open.

Five reasons, in the order that decided it:

1. **The reachable half is already exactly right.** JNI `DefineClass` with a NULL
   name and `ClassLoader.defineClass(null, …)` both arrive as `String::new()` and
   both now render `<Unknown>`, which §3 measured on HotSpot. The only input the
   patch changes is `defineClass("")` — an explicitly empty binary name, which is
   not a valid binary name at all, which `javac` cannot emit and which no
   framework in the corpus produces. The patch buys fidelity on an input nobody
   has.
2. **The observable is one word inside an exception message.** Not a type, not a
   thrown-or-not decision, not a field value. `UnsupportedClassVersionError`'s
   sentence already carries the type and the version; §3's five HotSpot arms
   differ from ours only in `<Unknown>` versus a doubled space. Nothing in the
   tree parses that message, and if anything ever does, the message it will parse
   is the reachable one.
3. **The cost is a signature change on the class-definition funnel.**
   `define_class_with_options` is the one entry every define path converges on —
   classpath loads, `ClassLoader.defineClass`, `Lookup.defineClass`,
   `defineHiddenClass`, JNI. Threading `Option<&str>` through it plus
   `read_optional_internal_name` and `jni_define_class` puts three files in two
   crates, two of them outside any one lane, in the blast radius of a placeholder
   spelling. The campaign's own record of what a define-path edit costs when it
   goes wrong is W7-82-forname-duplicate-define.md.
4. **It cannot be closed by a scheduled assertion in the shape it would need
   one.** A vector could subclass `ClassLoader` and call
   `defineClass("", b, 0, b.length)` on a hand-stamped `69.65535` file, but the
   check it would assert is "CratonVM prints a doubled space where it currently
   prints `<Unknown>`" — a fixture whose only purpose is to pin the fix, on an
   input the JVMS does not admit. The falsifier this record already carries ("a
   nameless define of a 69.65535 class file must say `<Unknown>`, not blank") is
   the assertion worth having, and it covers the half that ships.
5. **Declining loses no information.** §3.1 states the divergence in place, at
   `read_optional_internal_name`'s own trade-off, and names the shape
   (`Option<&str>`, not a second sentinel string) for anyone who ever needs it.
   That is the correct end state for a knowing, documented, unobservable
   inaccuracy.

**What would reopen it**, stated so this is a decision and not a shrug: a real
consumer that branches on the placeholder text; HotSpot changing either spelling
so ours is wrong on the *reachable* half too; or `read_optional_internal_name`
being refactored to `Option<String>` for an unrelated reason, at which point
rendering `None` as `<Unknown>` and `Some("")` as `""` is free and should be
taken.

**Nothing else in this record is open.** §4's four parts (A–D) are applied or
measured unnecessary, §5's hazards are checked, and the remaining caveats are
labelled unmeasured rather than unresolved — §"What is not claimed" is a build
dependency, not a defect.

## Falsifier

Unchanged from W7-28's, plus:

* `cratonvm --enable-preview -cp . Q` must print `ran-Q` where the pre-change
  binary printed `error: unexpected argument`, and `cratonvm -cp . Q` must now
  refuse it — with `Q.class` hand-stamped to `69.65535` as in §2. That single
  fixture exercises the flag, the gate and the new message without depending on
  any preview API.
* `java --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED` printing
  `PreviewFeatures.isEnabled` must give the same answer under `cratonvm` as under
  `java` on **both** arms. One arm agreeing proves nothing: a hardcoded `0` also
  passes the no-flag arm, which is exactly the state this record replaces.
* A nameless define of a 69.65535 class file must say `<Unknown>`, not blank.

## 7. 2026-08-12, second pass: the three falsifiers were RUN. Two pass; the third is a live row.

The build this record said it needed exists (`scratchpad/bin/cratonvm-f8.exe`,
with `cratonvm-control-44044c7e2.exe` as the pristine-dev control). Every
falsifier above was run against both, with HotSpot Adoptium 25.0.3.9 beside
them, on a `69.0` class file with bytes 4..5 overwritten to `FF FF`.

**Falsifier 1 — the flag and the gate. PASSES.**

```
HotSpot                    -> UnsupportedClassVersionError: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'
HotSpot --enable-preview   -> ran-Q
f8                         -> linkage error: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'
f8 --enable-preview        -> ran-Q
```

`--enable-preview` parses where §2 measured `error: unexpected argument`, the
gate refuses, and the sentence is HotSpot's word for word. The control binary
behaves the same, so parts A/B/C were already on `dev` at `44044c7e2`.

**Falsifier 2 — the two bits must agree, and one arm proves nothing. PASSES.**
`jdk.internal.misc.PreviewFeatures.isEnabled`, reflected through
`--add-exports=java.base/jdk.internal.misc=ALL-UNNAMED`: `false` without the
flag and `true` with it — on HotSpot, on `--real-jdk` and on `--jdk-only`.

**Falsifier 3 — `<Unknown>`. The TEXT passes. The TYPE does not, and that is a
live defect.** `ClassLoader.defineClass(null, b, 0, b.length)` over the same
bytes, which §3 established is the reachable analogue of JNI `DefineClass` with
a NULL name:

```
HotSpot : java.lang.UnsupportedClassVersionError: Preview features are not enabled for <Unknown> (class file version 69.65535). Try running with '--enable-preview'
f8      : java.lang.ClassFormatError: : defineClass1: Linkage(UnsupportedClassVersionError { class_name: "", message: "Preview features are not enabled for <Unknown> (class file version 69.65535). Try running with '--enable-preview'" })
```

Identical on `--jdk-only`, on `--real-jdk` and on the control — **pre-existing,
not this wave's**, and not the `<Unknown>` vs `""` question §6 declined.

**The site.** §4(C) fixed the mapping at `class_manager.rs`, which is the
main-class road. The `ClassLoader.defineClass` road re-wraps below it:
`native_classloader_define_class1` (`native-builtins/src/lang_system.rs:5011`)
ends every failure arm with

```rust
Err(define_class_format_error(&name, "defineClass1", msg))
```

so a typed `LinkageError::UnsupportedClassVersionError` is flattened back into
`ClassFormatError` — the very collapse this record's §1 table lists as verified
("`class_manager.rs:5252` maps every reader error to `ClassFormatError`") — and
a Rust `Debug` rendering is carried into a Java exception message, `class_name`
field and all. `defineClass2` (`:5016`) takes the same tail.

**Why it matters more than a spelling.** A caller catching
`UnsupportedClassVersionError` — which is what every container does when it
probes whether it can load a bundle, and exactly the `catch` HotSpot's message
invites — does not catch this. §4(C)'s exhaustiveness census over `match`es on
`LinkageError` was correct and is not the gap; the gap is a road that does not
propagate the enum at all.

**Not fixed here** (this pass adjudicates records and does not build), and
deliberately **not given a new record number**: two other lanes were running and
`W7-94` would collide. The fix is to give `define_class_format_error` a
pass-through for an already-typed `VmError::Linkage`, and it needs the same
exhaustiveness care §4(C) applied — plus a falsifier, which this section now is.

## 8. 2026-08-12 (A34) — re-run on a fresh binary, and the aperture correction

Method, so the provenance is unambiguous: `P.class` and `Q.class` compiled at
`69.0` and hand-stamped `ff ff` at bytes 4..5 (`od -An -tx1 -N8` reads
`ca fe ba be ff ff 00 45`), Microsoft JDK 25.0.3.9 as oracle, CratonVM rows on
`scratchpad/bin/cratonvm-merged-dev.exe` with `--java-home` passed.

**Falsifier 1 — the flag and the gate. PASSES, again.**

```text
HotSpot                        -> UnsupportedClassVersionError: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'
HotSpot --enable-preview       -> ran-Q
cratonvm --jdk-only            -> linkage error: Preview features are not enabled for Q (class file version 69.65535). Try running with '--enable-preview'
cratonvm --jdk-only --enable-preview -> ran-Q
```

**Falsifier 3 — and the correction that matters.** §7 printed one row. Printing
the named define beside it changes what the finding is:

| define call | HotSpot | CratonVM (`--jdk-only` and `--real-jdk`, identical) |
|---|---|---|
| `defineClass(null, b, 0, b.length)` | `UnsupportedClassVersionError: … for <Unknown> …` | `ClassFormatError: : defineClass1: Linkage(UnsupportedClassVersionError { class_name: "", message: "… <Unknown> …" })` |
| **`defineClass("P", b, 0, b.length)`** | `UnsupportedClassVersionError: … for P …` | **`ClassFormatError: P: defineClass1: Linkage(UnsupportedClassVersionError { class_name: "P", message: "… P …" })`** |

Both rows carry HotSpot's sentence intact inside a Rust `Debug` rendering, and
both are the wrong exception type. **The nameless case is not special here.**
§7 reached the right site by the wrong road: it found the defect while chasing
`<Unknown>`, and then described it as living on the nameless-define path,
which is one row of a two-row table. The generalisable version:

> A defect found while running a falsifier for something else inherits that
> falsifier's framing. Before filing it, vary the input the falsifier was
> holding fixed — here, the name argument — and check the finding is still
> about what you think it is about.

**Why the type matters more than the spelling**, restating §7 because it is
correct and now applies to a wider surface: a caller that catches
`UnsupportedClassVersionError` — which is what a container does when it probes
whether it can load a bundle, and exactly the `catch` HotSpot's message invites
— does not catch a `ClassFormatError`. That is now known to apply to every
`ClassLoader.defineClass` caller, not to the nameless ones.

**Disposition: fixed in source, NOT BUILT, and this section is the falsifier
for the rebuild.** After a build carrying `67146db71`, both rows above must
read `java.lang.UnsupportedClassVersionError` with HotSpot's sentence and **no**
`Linkage(...)`/`class_name:` `Debug` residue, in both modes. If only the named
row converts, the recovery is keying on a non-empty `class_name` and the
nameless road still flattens — which would be §7's framing coming true after
the fact, and the one outcome that would make its narrow aperture the right one.

## What is not claimed

Nothing was rebuilt; `cargo build`, `check`, `test`, `clippy` and `fmt` were all
withheld deliberately. The HotSpot messages in §3, the class-file headers in §4,
the pre-change binary's behaviour in §2, the Cargo dependency facts in §1 and the
`LinkageError` match census in §4 are observations. They do **not** establish
that the edited source compiles, that `--enable-preview` parses, that the two
bits agree at runtime, or that `java/lang/UnsupportedClassVersionError` can be
materialized on any given boot path.
