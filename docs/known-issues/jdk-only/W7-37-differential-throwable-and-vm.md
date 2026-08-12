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

> **Part 3 added 2026-08-12** (see the bottom of this file): the four out-of-file
> items are adjudicated — one applied and found to be **two** sites rather than
> one, one declined, one reported with a 30-site/13-file blast radius, one
> confirmed as needing nothing. Part 3 also **corrects this record's own claim**
> that the funnel covers the JIT cast helper: it did not, and after Part 2 landed
> the interpreter and the JIT printed different text for the same refusal. Also
> new: the four scheduled assertions this record had none of.

> **Part 4 added 2026-08-12** (see below): one of those new assertions is **RED**.
> The `ArrayStoreException` tier-parity check reads `cold=[java.lang.Integer]
> hot=[no-throw]` — the JIT-compiled `aastore` does not refuse the store at all,
> because `jit/src/x64/bytecode_walk.rs` lowers `aastore` inline and **never calls
> `jit_aastore`**, the helper Part 3 routed through the funnel. `VM.arrayStore`'s
> "after" cell in the table below therefore holds only for the interpreter. The
> `ClassCastException` rows are unaffected — `checkcast` does call its helper.

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

## Part 3 — the four out-of-file items, adjudicated 2026-08-12 (unbuilt, unrun)

Taken by the lane that owns `vm/src/jit/helpers.rs`. Item 2 is applied, item 3
is declined with a reason, item 1 is reported with its blast radius, and item 4
needed nothing. **Two of the record's own claims are corrected below.**

### Item 2 — APPLIED, and it was TWO sites, not one

`jit_aastore` now raises `RuntimeError::ArrayStoreException { message: elem_cls }`
through `throw_runtime_error` instead of calling `create_exception_object`
directly, so the funnel converts `java/lang/Integer` to `java.lang.Integer`
exactly as it does for the interpreter's `aastore`. The failure behaviour is
unchanged: if no thread is available, or the funnel degrades to an
`InternalError`, the code falls through and performs the store rather than
corrupting VM state, which is what the pre-existing comment there promises.

**The correction.** §"Which paths newly print a different string" above lists
*"the JIT cast helper's `class … cannot be cast to class …`"* among the paths the
funnel newly covers. **It did not cover it.** `jit_checkcast`
(`vm/src/jit/helpers.rs`, the `class {} cannot be cast to class {}` `format!`)
builds its `ClassCastException` with `create_exception_object` too, so it never
reached `throw_runtime_error` and never acquired the module/loader parenthetical.
The interpreter's `checkcast` raises a `RuntimeError` and did acquire it — so
after Part 2 landed, the same cast refusal printed **different text depending on
whether the enclosing method had tiered up**, which is strictly worse than
either wording alone. That site is now routed through the funnel as well. Its
`msg` is already in the exact two-operand shape `split_cast_operands` requires,
and the rewrite fails open, so a user-defined loader keeps today's wording on
both tiers rather than on one.

Both routings are in one file and neither adds a lock: in both functions the
`class_manager.read()` guard is a temporary inside the `let` that computes the
name and is dropped before the call.

### Item 3 — DECLINED, and the reason is now stronger than "not urgent"

The record's own words for it are *"functionally equivalent, so not urgent"*.
Declined outright, on two grounds. First, item 2 above **removes the premise**:
with the two JIT helpers routed through the funnel, all four raise sites now
reach one message builder, and having them call it directly instead of feeding
it text is a spelling change with zero observable difference. Second, the
refactor spans `runtime/interpreter/opcodes.rs` — a file several concurrent
lanes touch — for no behaviour, which is exactly the trade this campaign has
learned not to make.

### Item 1 — REPORTED, not applied. The blast radius is 30 sites in 13 files

`RuntimeError` lives in `types/src/error.rs`, which no natives/VM lane owns, and
changing `ClassCastException`'s payload from `{ message: String }` to two
`ClassId`s reaches every construction site and every match arm. Counted
2026-08-12 (`RuntimeError::ClassCastException` and `ClassCastException {`
tree-wide, excluding `target/`):

| file | sites |
|---|---|
| `native-collections/src/lib.rs` | 7 |
| `vm/src/runtime/exceptions.rs` | 5 |
| `native-builtins/src/phases_late/nio_file.rs` | 4 |
| `types/src/error.rs` | 3 (declaration, `as_java_throwable` arm, one test) |
| `native-builtins/src/lang_class.rs` | 2 |
| `native-builtins/src/atomic_updater.rs` | 2 |
| `vm/src/vm/vm_util.rs` · `runtime/interpreter/opcodes.rs` · `interpreter/lambda.rs` · `interpreter/jit_bridge.rs` | 1 each |
| `native-collections/tests/mock_arraylist.rs` | 1 |
| `native-builtins/src/xnio_async.rs` | 1 |
| `native-builtins/src/phases_late/collections.rs` | 1 |

Eight of the thirteen files are outside any one lane's ownership, and **most of
the thirty are not casts at all** — the checked-collection refusals, the
`AtomicReferenceFieldUpdater` type gate, `nio_file`'s attribute-view refusals
and `xnio_async`'s helper all raise free text with no two-operand structure, so
they have no two `ClassId`s to carry. A `ClassCastException { from: ClassId, to:
ClassId }` variant therefore cannot replace the existing one; it has to be a
**second, additive variant** used only by the four VM-minted cast sites, with the
string variant kept for everything else. That is a different and much smaller
change than the record prescribed, and it is the form a taker should scope.
Recorded, not half-landed.

### Item 4 — nothing needed, confirmed

Re-checked: the `java/lang/Throwable` registrations are still in
`native-builtins/src/lib.rs` and still point at the `lang_misc.rs` bodies. No
registration moved.

### Coverage — the instrument this record did not have

§"In-tree callers asserting the old wording" established that **nothing** in the
tree asserted either message, which is why eight measured divergences could be
fixed in source with no scheduled witness at all. `regression-suite/src/RExceptions.java`
(`CORE_CLASSES`, default invocation) now carries five — the file moves **13 → 25**
counting L16's and W7-33's checks from the same pass:

* `ArrayStoreException`'s message is exactly `java.lang.Integer` — fails on the
  slashed internal name.
* `ClassCastException`'s message `startsWith("class java.lang.String cannot be
  cast to class java.lang.Integer (")` — the half that fails on the bare
  two-operand form, and is robust to the parenthetical's own wording.
* `ClassCastException`'s message is exactly HotSpot's measured string, joint
  module/loader clause included. **Split from the check above on purpose so a
  red localises:** if only this one fails, the divergence is in the
  parenthetical (most likely `class.module_name` reading unnamed where HotSpot
  says `java.base`), not in the rewrite firing at all.
* Each of `ArrayStoreException` and `ClassCastException`, re-asserted after
  **1200** calls to the helper that raises it — past the default JIT invocation
  threshold of 500. Those two checks are the only ones in the file that can see
  a JIT/interpreter split, and they are trivially true on HotSpot, so they
  cannot flake on the oracle arm.

The equality check is deliberately exact rather than `contains`, because a
partial match on a message whose whole point is byte parity reads as good news
while measuring a fraction of it.

The two warm-up assertions also cover a second thing the funnel cannot: whether
JIT and interpreter agree that a refusal happens **at all**.
`aastore_element_assignable` fails open on imprecise type info, so a JIT arm that
declined to throw where the interpreter throws would now be a red rather than a
silent heap-type-confusion.

---

## Part 4 — the assertion in the paragraph above fired, and it is not a wording drift

**Measured 2026-08-12** on the frozen wave binary
(`scratchpad/bin/cratonvm-wave-full.exe`, `--jdk-only`, same JDK 25 on both
sides). `RExceptions` is RED at the ASE tier-parity assertion; HotSpot is
`PASS RExceptions (25 checks)` on the same class file.

| reading | value |
|---|---|
| interpreted (`aseCold`) | `java.lang.Integer` |
| JIT-compiled, i=500 (`aseHot`) | `no-throw` |
| interpreted **and** JIT-compiled CCE | HotSpot's string, byte-identical, stable across 1200 iterations |

`no-throw` is the fixture's own sentinel for "the store completed". So the two
tiers do not print different text for the same refusal — **the compiled tier does
not refuse.** Part 3's item-2 conclusion (both mint sites now reach
`throw_runtime_error`) is correct and is *not* the residual; the residual is that
one of those two sites is unreachable.

### Why the funnel fix could not have covered it

`jit/src/x64/bytecode_walk.rs:1778` (`0x53`) lowers `aastore` **inline** — null
check, bounds check, `jit_satb_pre_write_barrier`, `MOV QWORD [array + index*8 +
HEADER_SIZE], val`, card mark — and never calls `self.helpers.aastore`. The
`jit_aastore` function in `vm/src/jit/helpers.rs` is dead code on x64. Part 3
routed its ASE through the funnel; nothing routes anything through
`jit_aastore`.

The arm says why, in a comment written at R20 / HIGH-5:

> ArrayStoreException note: the current `jit_aastore` helper does NOT enforce
> the ASE check (the interpreter does it via `set_array_element`). This inline
> path matches the helper's behavior exactly — no regression.

That was true the day it was written. The JVMS §aastore covariance check was
later added to `jit_aastore`, in a different crate, and the "matches exactly"
premise was falsified with nothing to notice: **a premise stated in a comment is
not a compile-time link**, so the two paths diverged silently and stayed diverged
until a fixture asked. The CCE half survives only because `checkcast` (0xc0)
*does* call its helper — `self.helpers.checkcast` — which is exactly why one of
the two moved and the other did not.

Second-order consequence, worse than the message: with the check skipped, a
JIT-compiled `String[] ← Integer` store **succeeds**, so a heap slot the class
declares as `String` now holds an `Integer`. That is a type confusion the GC's
reference-array invariants and every later `aaload`/`checkcast` inherit. The
message parity this record is about is the cheap half of the bug.

### The codegen change (OUT OF FILE — `jit/` is not this lane's)

Keep the inline null and bounds checks: the null-check dataflow elision and BCE
key on them, and they guard the helper's own header reads. Move everything from
the covariance check down into `jit_aastore`, which already performs the check,
the SATB pre-write barrier, the store and the write barrier in the right order.
`jit_aastore` returns **void**, so RAX carries no `i64::MIN` sentinel;
`jit_dispatch_threw` (`self.helpers.dispatch_threw`) peeks the out-of-band signal
non-destructively and `SHL RAX, 63` maps its `0`/`1` onto the sentinel
convention `emit_post_invoke_exception_check` already routes — via a **reason-9
precise frame** when the store sits inside a protected range, which is what puts
the ASE into the compiled method's *own* `catch` rather than its caller's. No new
helper, no new helper-table slot: `aastore` is already a populated field
(`jit-api/src/lib.rs:757`, wired at `vm/src/jit/helpers.rs`'s table literal) and
has simply had no caller since R20.

`emitted_checkcast_throw = true` forces `has_dispatch` (`jit/src/x64/driver.rs`),
which is what sets the `JIT_THREAD` TLS `jit_thread_mut()` needs; without it
`jit_aastore` takes its documented "no thread available" arm and **performs the
store anyway**, reproducing today's `no-throw` through a second route. The flag
is shared with `checkcast` deliberately — `checkcast` and `aastore` are one JVMS
type-check rule, and the flag's job ("this compile can stash a VM-minted
type-error throwable and bail via the sentinel") is the same for both. A taker
who prefers a distinct `emitted_aastore_throw` must add it in three places
(the struct in `jit/src/x64` and its two constructors) plus the `has_dispatch`
disjunction.

**Cost, stated plainly and unmeasured:** this puts a Rust-boundary call back on
the hottest reference-store path in the VM (`ArrayList.add`, every hash-table
`put`), and `aastore_element_assignable` walks the class hierarchy on each one.
It trades R20 / HIGH-5's throughput win for JVMS conformance. The perf-preserving
form is a per-site monomorphic inline cache — guard `class_id_of(array)` and
`class_id_of(val)` against the pair the helper last accepted, and call only on a
miss — which is the "type-narrowing infrastructure (not yet tracked in this JIT)"
the R20 comment already named. That is a separate piece of work; do not let it
hold the correctness fix, and do not land the correctness fix without an A/B on
a store-heavy benchmark.

### The reflective twin, checked

`Array.set` and `aastore` are one JVMS rule implemented twice, so the native was
measured too. Both VMs agree — `Array.set(String[], 0, Integer.valueOf(1))`
throws `IllegalArgumentException` (argument type mismatch), *not*
`ArrayStoreException`, on HotSpot 25 and on CratonVM, interpreted and past the
JIT threshold. The reflective side needs nothing.

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
