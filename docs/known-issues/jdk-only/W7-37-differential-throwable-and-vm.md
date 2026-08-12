# W7-37 — `Throwable`'s state machine, and the module/loader clause on a VM-minted cast refusal

**Status: EIGHT DIVERGENCES MEASURED AND FIXED IN SOURCE 2026-08-12, NOT REBUILT.**

Every measurement below was taken by running the already-built binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` against Temurin
`jdk-25.0.3.9-hotspot` on windows/x64 — one binary, the same class files on both
sides, `-Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US` pinned on
both. **Nothing here claims a source change works.** Every "before" row is an
observation; every "after" row is a claim about source.

Branch: `fix/differential-throwable-and-vm-20260812`.
Files changed: `native-builtins/src/lang_misc.rs`,
`vm/src/runtime/exceptions.rs`, and this record.

Predecessor: W7-32-round-2-differential-run (the run these eight rows come
from), W7-33-differential-dead-sections (the same probe's two dead sections,
whose lesson — a refusal can be *correct* code reading state we corrupted —
shaped every fail-open arm below).

---

## The eight rows

| observable | HotSpot | CratonVM before | after (source claim) |
|---|---|---|---|
| `Throwable.initCauseAfterCtorThrows` | `java.lang.IllegalStateException` | `no-throw` | `IllegalStateException` |
| `Throwable.initCauseTwiceThrows` | `java.lang.IllegalStateException:java.lang.IllegalArgumentException: c` | `no-throw:java.lang.IllegalArgumentException: c2` | `IllegalStateException:…: c` |
| `Throwable.selfCauseThrows` | `java.lang.IllegalArgumentException` | `no-throw` | `IllegalArgumentException` |
| `Throwable.addSuppressedSelfThrows` | `java.lang.IllegalArgumentException` | `no-throw` | `IllegalArgumentException` |
| `Throwable.suppressionDisabled` | `0:0` | `1:0` | `0:0` |
| `VM.classCast` | `java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Integer (java.lang.String and java.lang.Integer are in module java.base of loader 'bootstrap')` | `java.lang.ClassCastException: java.lang.String cannot be cast to java.lang.Integer` | HotSpot's string |
| `VM.checkcastToArray` | `java.lang.ClassCastException: class [I cannot be cast to class [Ljava.lang.String; ([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')` | `java.lang.ClassCastException: [I cannot be cast to [Ljava.lang.String;` | HotSpot's string |
| `VM.arrayStore` | `java.lang.ArrayStoreException: java.lang.Integer` | `java.lang.ArrayStoreException: java/lang/Integer` | `java.lang.Integer` |

Note the second row: the divergence is not only the missing throw. Because the
second `initCause` silently succeeded, CratonVM's *cause* was `c2` where
HotSpot's is still `c` — the first cause had been overwritten.

---

## Part 1 — `Throwable`

### Why these were missing at all

`initCause`, `addSuppressed` and `getSuppressed` are registered as natives on
`java/lang/Throwable`. A native on a real JDK class does not add a fallback; it
**replaces** the method for every instance. So the refusals the JDK specifies do
not exist unless the shadow writes them, and this shadow did not: `initCause`
was an unconditional field write, and `addSuppressed` returned silently on a
null argument, returned silently on a self argument, and appended into the list
regardless of what was there.

### The rule, per method, with the javadoc sentence

`Throwable.initCause(Throwable)`:

> `@throws IllegalStateException` if this throwable was created with
> `Throwable(Throwable)` or `Throwable(String,Throwable)`, or this method has
> already been called on this throwable.

> `@throws IllegalArgumentException` if `cause` is this throwable. (A throwable
> cannot be its own cause.)

`Throwable.addSuppressed(Throwable)`:

> `@throws IllegalArgumentException` if `exception` is this throwable; a
> throwable cannot suppress itself.

> `@throws NullPointerException` if `exception` is `null`.

> If suppression is disabled, this method does nothing other than to validate
> its argument.

`Throwable.getSuppressed()`:

> If suppression was disabled … an empty array will be returned.

### Order is part of the specification

The JDK runs `initCause`'s **already-set test first**. That is why
`new RuntimeException().initCause(itself)` is an `IllegalArgumentException` and
not an `IllegalStateException`: the sentinel still reads "unset", so the first
test passes and the self test fires. Reversing them would raise a refusal of the
right *shape* and the wrong *type*, and a mistyped refusal sends a caller down
the wrong `catch` branch as surely as a missing one — the same defect another
lane measured today in `Signature.getInstance`, which accepted every algorithm
name and failed later, past every `catch (NoSuchAlgorithmException)`.

`addSuppressed` runs both argument validations **before** the disabled check.
Measured, not assumed:

```
disabled.addSelf=java.lang.IllegalArgumentException: Self-suppression not permitted
disabled.addNull=java.lang.NullPointerException: Cannot suppress a null exception.
disabled.count=0
```

### HotSpot's exact messages (measured, JDK 25)

| refusal | type | message | `getCause()` |
|---|---|---|---|
| second `initCause` | `IllegalStateException` | `Can't overwrite cause with java.lang.IllegalArgumentException: c2` | the receiver |
| `initCause(null)` after one call | `IllegalStateException` | `Can't overwrite cause with a null` | the receiver |
| `initCause(this)` | `IllegalArgumentException` | `Self-causation not permitted` | the receiver |
| `addSuppressed(this)` | `IllegalArgumentException` | `Self-suppression not permitted` | the receiver |
| `addSuppressed(null)` | `NullPointerException` | `Cannot suppress a null exception.` | `null` |

The trailing period on the NPE message is HotSpot's, not a typo. The refusals
carry the receiver as their own cause (`throw new IllegalStateException(msg,
this)`), which is observable, so they are constructed as real throwables rather
than raised as message-only `RuntimeError`s — falling back to message-only only
if that construction fails, because the *type* is the half that must survive.

### The state both checks read is already correct on CratonVM

This is the measurement that made the fix safe to write, because both checks
test a raw field rather than a flag of our own. Same probe, both VMs,
`--add-opens java.base/java.lang=ALL-UNNAMED`, reading `Throwable.cause` and
`Throwable.suppressedExceptions` reflectively:

| field state | HotSpot | CratonVM |
|---|---|---|
| `cause` after `new RuntimeException("m")` | `== this` | `== this` |
| `cause` after `new RuntimeException("m", c)` | not self | not self |
| `cause` after `initCause(null)` | `null` | `null` |
| `suppressedExceptions` default | `java.util.Collections$EmptyList` | `java.util.Collections$EmptyList` |
| `suppressedExceptions`, suppression disabled | `null` | `null` |

The real JDK constructor bytecode runs on both, so `cause != this` and
`suppressedExceptions == null` mean on CratonVM exactly what they mean on
HotSpot. The shadows were reading the right fields and ignoring them.

### The one place that was NOT already correct, and why it had to be fixed first

Making `suppressedExceptions == null` mean "suppression disabled" is only safe
if every throwable that *is* suppression-enabled has a non-null list.
VM-**minted** throwables did not:

| throwable | `suppressedExceptions == null`, HotSpot | CratonVM |
|---|---|---|
| VM-raised `NullPointerException` | `false` | **`true`** |
| `ClassNotFoundException` from `Class.forName` | `false` | **`true`** |
| VM-raised `ArithmeticException` | `false` | `false` |
| VM-raised `ClassCastException` | `false` | `false` |

Shipping the disabled rule without closing that would have turned
`addSuppressed` into a silent no-op on VM-minted exceptions — and
`addSuppressed`/`getSuppressed` are what try-with-resources compiles into, so
the visible symptom would have been: every `close()` failure vanishing whenever
the *body* threw a VM-raised exception, with nothing looking wrong.
`mirror_throwable_field_initialisers` in `vm/src/runtime/exceptions.rs` now
mirrors the JDK's two declared field initialisers on anything
`create_exception_object` builds. It reads the real
`java.lang.Throwable.SUPPRESSED_SENTINEL` static rather than parking any empty
list, so the JDK bytecode's own identity tests stay valid, and does nothing
before `Throwable.<clinit>` has populated it.

The `cause` half of that mirror is deliberately narrower — it fills in only a
slot that was **never written** (an unset reference slot reads back as `Int(0)`,
not `Object(None)`). A stored null is left alone, because several JDK throwables
null their cause on purpose: `ClassNotFoundException()` and
`InvocationTargetException()` both chain to `super((Throwable) null)`, HotSpot
then refuses `initCause` on them, and that refusal was measured on both VMs
(`ite.initCauseThrows=IllegalStateException`). Overwriting the null would have
converted a specified refusal into a silent success — an over-throw's mirror
image, and just as wrong.

### Every arm that cannot read a verdict fails open

W7-33 closed two sections of this same probe that looked like CratonVM
over-throwing and were real JDK code refusing state we had corrupted. The
refusals added here are therefore gated so that an unreadable signal never
produces a throw:

* `initCause`'s already-set test treats a stored null as "set" **only when the
  receiver's class actually declares/inherits `cause`** — `read_throwable_field`
  answers the same `Object(None)` for a class that has no such field at all —
  and treats an unset (`Int(0)`) slot as no verdict.
* `addSuppressed`'s disabled test requires all three of: the receiver declares
  the field, the field reads as a genuine `Object(None)`, and
  `SUPPRESSED_SENTINEL` is populated. Any failing clause appends, because an
  extra entry in `getSuppressed()` is visible and a dropped one is not.

`getSuppressed` needed no change: a null field is not an array, so its existing
empty-array tail was already the specified answer. `Throwable.suppressionDisabled`
measured `1:0` because `addSuppressed` *wrote an array into the null field*, not
because of anything on the read path.

---

## Part 2 — the three VM-minted messages

### HotSpot's exact strings, including the loader/module clause

Both operands in the same module (`VM.classCast`, `VM.checkcastToArray`):

```
class java.lang.String cannot be cast to class java.lang.Integer (java.lang.String and java.lang.Integer are in module java.base of loader 'bootstrap')
class [I cannot be cast to class [Ljava.lang.String; ([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
```

Operands in different modules — measured with a default-package class so the
app-loader/unnamed-module half of the format is on the record too:

```
class ThrProbe cannot be cast to class java.lang.String (ThrProbe is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
```

`ArrayStoreException` is the offending element's external name and nothing else,
with arrays in **descriptor** form:

```
java.lang.ArrayStoreException: java.lang.Integer
java.lang.ArrayStoreException: [Ljava.lang.Integer;
```

Four things are load-bearing in there and each was checked rather than recalled:

1. The `class ` prefix on **both** operands.
2. The joint clause (`X and Y are in …`) when the two klasses share a module,
   versus two `; `-separated `is in` clauses when they do not — HotSpot's
   `SharedRuntime::generate_class_cast_message` picking between
   `Klass::joint_in_module_of_loader` and `Klass::class_in_module_of_loader`.
3. `unnamed module of loader 'app'` has no `module ` word before it; a named
   module does (`in module java.base of`).
4. Arrays render as descriptors (`[I`, `[Ljava.lang.String;`), **not** the JEP
   358 spelling (`int[]`, `java.lang.String[]`) the helpful-NPE messages in this
   same file use. Those are two different HotSpot renderers and this file now
   has both.

A primitive-component array's module is java.base and its loader is bootstrap —
`int[].class.getModule().getName()` measures as `java.base` on both VMs — which
is why the array arm reads the module off the bottom klass.

### Why a slashed name is a defect and not untidiness

`ArrayStoreException: java/lang/Integer` is not merely un-HotSpot-like. The same
defect in the sibling `checkcast` message once made mockk's `JvmAutoHinter`
regex — `cannot be cast to (class )?(.+/)?(.+?)( \(...\))?$` — capture `String`
instead of `java.lang.String`, so its `Class.forName` threw
`ClassNotFoundException: String`; the comment recording that sits at the
`checkcast` raise site in `vm/src/runtime/interpreter/opcodes.rs`. Every caller
that feeds an `ArrayStoreException` message to `Class.forName` has that hole
today. This campaign already carries a standing rule that a mistyped or
mis-worded VM exception is a real defect, and a separate record about
`NoClassDefFoundError` wording being misread as a `<clinit>` verdict.

### Where the rewrite lives, and what it can and cannot change

At `throw_runtime_error` in `vm/src/runtime/exceptions.rs` — the single funnel
every VM-raised `RuntimeError` passes through — rather than at the raise sites,
because those are other lanes' files (see "out of file" below). Applied after
every debug dump, so `CRATONVM_DBG_RTERR` still shows the raise site's own text.

**Paths that newly print a different string:** exactly those whose
`ClassCastException` message is already a bare two-operand cast refusal (the
interpreter's `checkcast`, `interpreter/lambda.rs`'s argument check, the JIT
cast helper's `class … cannot be cast to class …`), plus `ArrayStoreException`s
whose message is still a slashed internal class name (the interpreter's
`aastore`, and the `arraycopy` element check in `interpreter.rs`).

**Paths returned byte-identical:** every checked-collection refusal, the
JIT-panic bridge in `interpreter/jit_bridge.rs` (whose payload merely *contains*
`ClassCastException`), and any message with whitespace inside an operand. The
splitter requires the whole message to be nothing but the two operands, which
also makes the rewrite idempotent — an already-rewritten message ends in ` (…)`,
so its right operand contains a space and it is skipped.

### The one case deliberately left uncovered

A class defined by a **user-defined loader**. HotSpot renders those as
`<loader class name> @<identity hash>` (or `'<name>' @<hash>`), and the identity
hash is a value we cannot reproduce. Putting a wrong address into a message that
log-scrapers read is worse than the plain wording, so `loader_name_and_id`
answers only for the three built-in loaders (`'bootstrap'`, `'platform'`,
`'app'`) and any cast involving a user-loader class keeps today's message
unchanged. That leaves webapp/OSGi-loaded classes on the old wording — the
honest state, and the reason it is written down here rather than papered over.

### The by-name lookup, and why it is narrowed

`RuntimeError::ClassCastException`'s entire payload is a pre-rendered
`message: String`; a two-`ClassId` variant would have to live in
`types/src/error.rs`, which is another lane's file this wave. So the operands
are resolved back from their names, using `ClassManager::find_unique_class_by_name`
— whose own doc marks it as the lookup that is safe for diagnostics carrying no
initiating loader, *because* it answers `None` rather than guessing when two
loaders defined the same name — and only then the current frame's loader, which
is the loader that resolved the `checkcast` constant-pool entry. Nothing
dispatches on the answer; it decorates a message. But this campaign has closed
five separate defects that were all "bound by NAME", so the binding is a
narrowed, deliberate choice rather than a convenience.

---

## In-tree callers asserting the old wording

**None found.** Searched `regression-suite/`, `probes/`, `apps/` and
`vm/src/vm/tests.rs` for `cannot be cast to` and for `ArrayStoreException`, and
the whole tree's `*.rs` for an assertion on either message:

* every `cannot be cast to` hit in `regression-suite/`, `probes/` and `apps/` is
  in a **comment or a results table** (`RJitArrayTypecheck.java`,
  `ROverlaySystemGcStress.java`, `RTreeRangeGc.java`,
  `AnnotationProxyIsAnnotationProbe.java`, `CollectorPinProbe.java`,
  `G1ReferenceArrayValueProbe.java`, `LinkedHashMapNodeProbe.java`,
  `MapConditionalMutatorProbe.java`, `PathSeparatorMapProbe.java`,
  `TreeMapCmpProbe.java`, `apps/h2database-suite-runner/RESULTS-20260723.md`) —
  documentation of a past symptom, not an assertion;
* `regression-suite/src/RExceptions.java` and `RJdkJni.java` catch
  `ArrayStoreException` by **type** and never read `getMessage()`;
* `probes/ArraycopyHeaderOffsetProbe.java` asserts the exception **class**;
* the only Rust test touching either message,
  `exceptions.rs::throw_class_cast_exception`, asserts the returned
  `MethodCallFailed` variant and never the string.

So nothing had to be corrected, and nothing was worked around. (Contrast the
`certificate_factory_p68` case another lane found today, where a test pinned the
defect it was meant to guard.)

---

## Recorded as out of file

These are correct-but-not-mine and are left for the owning lanes:

1. **`types/src/error.rs`** — `RuntimeError::ClassCastException` should carry the
   two `ClassId`s, not a pre-rendered string. That would delete the by-name
   re-resolution above outright and make the message exact for user-defined
   loaders too (the object's class is known at the raise site; only its *name*
   survives today).
2. **`vm/src/jit/helpers.rs`** (~line 5389) mints an `ArrayStoreException`
   **object directly** rather than through `RuntimeError`, so it does not pass
   the funnel and keeps the internal name. Same fix, one call site.
3. **`vm/src/runtime/interpreter/opcodes.rs`**, **`interpreter/lambda.rs`**,
   **`jit/helpers.rs`** — the four raise sites would be clearer calling a shared
   message builder directly instead of having their text rebuilt downstream.
   Functionally equivalent, so not urgent.
4. **`native-builtins/src/lib.rs`** holds the `java/lang/Throwable` native
   *registrations* (the implementations are in `lang_misc.rs`, which is where
   this wave's changes are). No registration change was needed; noted only so
   the next reader does not go looking for the natives in one file.

---

## Reproduce

```sh
javac -d /tmp/probes probes/ShadowDifferentialProbe.java
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java" \
  -Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US \
  -cp /tmp/probes ShadowDifferentialProbe > /tmp/hs.txt
./target/release/cratonvm.exe --real-jdk \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" \
  -Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US \
  -cp /tmp/probes ShadowDifferentialProbe \
  | grep -v '^\[cratonvm\]' | grep -v WARN > /tmp/cv.txt
diff --strip-trailing-cr /tmp/hs.txt /tmp/cv.txt | grep -E "Throwable|VM\."
```

The field-state and message tables above came from two throwaway probes run the
same way with `--add-opens java.base/java.lang=ALL-UNNAMED` on both sides; the
`--add-opens` is required or `Field.setAccessible` on `Throwable.cause` throws
`InaccessibleObjectException` on HotSpot.
