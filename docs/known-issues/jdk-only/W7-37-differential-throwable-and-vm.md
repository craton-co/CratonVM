# W7-37 — `Throwable`'s state machine, and the module/loader clause on a VM-minted cast refusal

> **RECONCILED 2026-08-16 (merge of `dev`).** Every `aastore_check`,
> `jit_aastore_check` and `HelperFnAastoreCheck` below names the spelling that
> was current when this record was written. The merge of `dev` into
> `claude/jdk-only-mode-completion-1351c0` settled on **`aastore_type_check`**
> (ABI field), **`jit_aastore_type_check`** (helper) and
> **`aastore_store_is_refused`** (the predicate the helper and the x64 inline
> lowering now share), and the slot is `required: true` -- the old
> `aastore_check == 0` fallback that routed the whole opcode to
> `helpers.aastore` no longer exists. `grep -rn 'aastore_check' --include=*.rs`
> returns nothing in the tree. The names here are kept as written; read them as
> history, not as a pointer to live code.

**Status: REBUILT AND RUN 2026-08-12 (lane B8). SIX OF EIGHT ROWS VERIFIED FIXED.
TWO ARE STILL DIVERGENT, AND ONE OF THOSE THIS RECORD CLAIMED AS FIXED.**

**Part 5 / lane B11 added 2026-08-12 — both remaining rows re-measured shape by
shape, and both are bigger than recorded.** Row 8 loses the covariance check for
**five distinct store shapes**, not one, and one of the five
(`Animal[] <- String`) is wrong **in the interpreter too**, so it is not a JIT
row at all. Row 7 is not "the array branch never executes": CratonVM emits
**three different strings** for an array-operand cast depending on tier and on
definition-index state. The A/B the codegen fix was blocked on is now run:
**a Rust-boundary type check per reference store costs 3.4x on this VM's own
store loop**, which is why §B11.4 designs a cache rather than proposing the
plain call. See Part 5.

> ## B8 — the first run of this record's own table against a binary
>
> Every previous pass on this record was source-level: the table's "after"
> column was a *claim about source*, said so, and stayed unverified. It is now
> executed. Binary `/c/craton/jdkonly-wave2-target/release/cratonvm.exe`
> `--jdk-only`, oracle Temurin `jdk-25.0.3.9-hotspot`, one probe class file run
> on both arms and the two stdouts diffed.
>
> ```
> $ diff hs37.txt cv37.txt
> 8c8
> < checkcastToArray=class [I cannot be cast to class [Ljava.lang.String; ([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')
> ---
> > checkcastToArray=[I cannot be cast to [Ljava.lang.String;
> 10c10
> < arrayStoreHot=java.lang.Integer
> ---
> > arrayStoreHot=no-throw
> ```
>
> **Six rows are byte-identical to HotSpot and are RETIRED**: all five
> `Throwable` rows (`initCauseAfterCtorThrows`, `initCauseTwiceThrows` — cause
> `c` survives, `selfCauseThrows`, `addSuppressedSelfThrows`,
> `suppressionDisabled=0:0`) plus `VM.classCast`, whose parenthetical matches
> including the joint module/loader clause. `addSuppressed(null)` →
> `NullPointerException` agrees too. Part 1 and Part 2 are done.
>
> **Row 7 `VM.checkcastToArray` is NOT fixed** — the table's "after" cell says
> "HotSpot's string" and the binary prints the bare two-operand form. This is a
> correction to this record, not a stale line number.
>
> **Row 8 hot / Part 4 is CONFIRMED LIVE on today's binary**, not merely on the
> frozen wave binary Part 4 measured. `RExceptions` is RED at exactly the
> predicted assertion:
> `AssertionError: ArrayStoreException text moved during warm-up at i=500:
> cold=[java.lang.Integer] hot=[no-throw]`, against
> `PASS RExceptions (25 checks)` on HotSpot. Part 4's diagnosis stands
> unaltered: the JIT lowers `aastore` inline and never calls `jit_aastore`, so
> the compiled tier performs the store. The heap type confusion is real today.
>
> ### Row 7's mechanism, isolated by experiment rather than inferred
>
> A six-case probe varying only whether each operand is an array:
>
> | case | operands | CratonVM |
> |---|---|---|
> | `D_obj_to_obj` | `String` → `Integer` | **byte-identical to HotSpot** |
> | `F_app_to_bootstrap` | `CastProbe` → `String` | **byte-identical**, split two-clause form and all |
> | `A_obj_to_refarr` | `String` → `[Ljava.lang.String;` | bare form |
> | `B_primarr_to_obj` | `[I` → `String` | bare form |
> | `C_primarr_to_refarr` | `[I` → `[Ljava.lang.String;` | bare form |
> | `E_refarr_to_refarr` | `[Ljava.lang.Integer;` → `[Ljava.lang.String;` | bare form |
>
> The rewrite is correct whenever **neither** operand is an array and fails
> whenever **either** is — including the split-clause path, which is the harder
> case and works. So `hotspot_class_cast_message` is not at fault;
> `klass_origin` (`vm/src/runtime/exceptions.rs`) returns `None` for an array
> display name, and one `None` collapses the whole message via `?`.
>
> The cause is an ordering one, and it makes existing code dead:
> `klass_origin` opens with `find_unique_class_by_name(display_name)?` — a
> lookup of *the array class itself* — and only *afterwards* parses the `[`
> prefix to find the component's module. That descriptor-parsing block, which
> is written correctly and even carries HotSpot's `bottom_klass` rationale, can
> only ever run for an array class already in the definition index, and the
> measurements above show none are. **The array support in this function has
> never executed.** Nomination in §B8.1 below.
>
> ### Scheduling: row 7 is invisible to this record's own vector
>
> `regression-suite/src/RExceptions.java` asserts the CCE text only for
> `String` → `Integer` — the one shape that works. Four of the six cast shapes
> above have no scheduled witness at all, which is why row 7 could be recorded
> as fixed and stay wrong. Row 8 *is* scheduled and is red. Nominated vector in
> §B8.2.

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
| `VM.checkcastToArray` | `java.lang.ClassCastException: class [I cannot be cast to class [Ljava.lang.String; ([I and [Ljava.lang.String; are in module java.base of loader 'bootstrap')` | `java.lang.ClassCastException: [I cannot be cast to [Ljava.lang.String;` | **NOT FIXED — see §B11.1** |
| `VM.arrayStore` | `java.lang.ArrayStoreException: java.lang.Integer` | `java.lang.ArrayStoreException: java/lang/Integer` | **interpreter only; the JIT does not throw at all — §B11.2** |

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

---

## §B8.1 — NOMINATION: `klass_origin` must parse the descriptor before it looks up

`vm/src/runtime/exceptions.rs`. The array branch is currently unreachable
because the function demands the array *class* be in the definition index
before it will look at the component. Resolve the component instead, and take
the loader from it.

OLD (exact, from `fn klass_origin`):

```rust
    let cm = shared.classes.class_manager.read();
    let class_id = cm.find_unique_class_by_name(display_name).or_else(|| {
        let frame_class = thread.frames.last()?.class_id;
        cm.find_class_by_name_for_class(display_name, frame_class)
    })?;
```

NEW:

```rust
    let cm = shared.classes.class_manager.read();
    // An array's own class need not be in the definition index — measured: no
    // array display name resolves, which made the `dims` block below dead code
    // and dropped every array-operand cast message back to the bare form
    // (W7-37 §B8). HotSpot reads module and loader off the BOTTOM klass, so
    // resolve that directly and let the primitive case answer without a lookup
    // at all.
    let dims = display_name.bytes().take_while(|b| *b == b'[').count();
    let lookup_name = match display_name[dims..].strip_prefix('L') {
        Some(component) => component.trim_end_matches(';'),
        // Primitive-component array (`[I`) — java.base / bootstrap, per
        // HotSpot's "klass is an array of primitives, module is java.base".
        None if dims > 0 => {
            return Some(KlassOrigin {
                module: Some("java.base".to_string()),
                loader: "'bootstrap'",
                loader_id: cratonvm_types::ClassLoaderId::Bootstrap,
            })
        }
        None => display_name,
    };
    let class_id = cm.find_unique_class_by_name(lookup_name).or_else(|| {
        let frame_class = thread.frames.last()?.class_id;
        cm.find_class_by_name_for_class(lookup_name, frame_class)
    })?;
```

With `lookup_name` resolving the component, the existing `dims`/`strip_prefix`
block further down becomes redundant and should be deleted along with its
`let name = class.name.to_string();` — the module it computes is now the module
of the class just resolved. **Do not** delete the `bottom_klass` comment; move
it to the new block, which is where it is now load-bearing.

Verification is the six-case probe in §B8: all six rows must go byte-identical
to HotSpot, and `D`/`F` must not regress.

## §B8.2 — NOMINATION: schedule the four unwitnessed cast shapes

`regression-suite/src/RExceptions.java` covers only `String` → `Integer`. Add
the four array shapes beside it — `A_obj_to_refarr`, `B_primarr_to_obj`,
`C_primarr_to_refarr`, `E_refarr_to_refarr` from §B8 — asserting full byte
parity with the HotSpot strings quoted there. They are trivially true on
HotSpot, so they cannot flake on the oracle arm, and they are the difference
between row 7 being adjudicable and being re-recorded as fixed a third time.

**Done in Part 5** (§B11.7), with one change: each shape is read in BOTH tiers,
because a single cold reading would have recorded two of them as fixed. See
§B11.1 for why.

---

# Part 5 — lane B11: both remaining rows, shape by shape, and the A/B

Binary `/c/craton/jdkonly-wave2-target/release/cratonvm.exe --jdk-only`, oracle
Temurin `jdk-25.0.3.9-hotspot`, same class files on both arms, all measured
2026-08-12. Probes are throwaway (`AastoreShapes`, `CastMsgs2/3`, `AastoreAB`);
the durable versions of the assertions are now in
`regression-suite/src/RExceptions.java`.

## §B11.1 — Row 7 is not a dead branch. It is three different strings.

§B8 concluded that `klass_origin`'s array support "has never executed". That is
too strong, and the weaker true statement is worse. Five cast shapes, each read
interpreted and again past the JIT threshold:

| shape | interpreted | JIT-compiled |
|---|---|---|
| `String` → `String[]` | bare | bare **in one program, HotSpot's FULL string in another** |
| `int[]` → `String` | bare | `class [I cannot be cast to class java.lang.String` — prefixes, **no parenthetical** |
| `int[]` → `String[]` | bare | prefixes, no parenthetical |
| `Integer[]` → `String[]` | bare | prefixes, no parenthetical |
| `String` → `Long[]` (array class never instantiated) | bare | bare |

So there are **three** wordings in play for one JVMS rule — the bare
two-operand form, a prefix-only form with the module/loader clause missing, and
(sometimes) HotSpot's exact string — and which one you get is a function of the
tier and of whether the array class happens to be in the definition index at
that moment, not of the cast.

The `String` → `String[]` row is the demonstration. In a program whose only
`[Ljava.lang.String;` is `main`'s own parameter it reads bare in both tiers; in
one that calls the same site in a tight 1200-iteration loop it reads HotSpot's
full string once compiled, reproducibly, three runs out of three. In
`RExceptions` it now **passes** — because that fixture's own
`static final Object AS_V_STRARR = new String[1]` puts `[Ljava.lang.String;`
into the definition index before `main` runs.

That is exactly the mechanism §B8.1 nominated, seen from the other side.
`find_unique_class_by_name(display_name)` on the array class does not *never*
resolve; it resolves iff something already defined that array class. Which means
row 7 does not fail deterministically, and **a probe that happens to allocate the
array type it is about will report it fixed.** Two of this record's three
"recorded as fixed" events are explained by that and by nothing else. §B8.1's
patch is still the right fix and its rationale is unaffected — resolving the
*component* removes the dependence on incidental index state — but its
justification should read "the array branch is reachable only by accident",
not "never executes".

Under `--nojit` all five shapes are stable across the 1200 iterations, which
locates the tier half of the instability in the JIT `checkcast` helper rather
than in the funnel.

## §B11.2 — Row 8 is five shapes, and one of them is not a JIT bug

Eleven `aastore` shapes, each with its own store site in its own method, each
read interpreted and again past the JIT threshold. HotSpot is correct on all
eleven in both tiers.

| shape | must | CratonVM interpreted | CratonVM JIT |
|---|---|---|---|
| `String[] <- Integer` | ASE | ASE | **no-throw** |
| `Number[] <- String` | ASE | ASE | **no-throw** |
| `Animal[] <- String` (interface component) | ASE | **no-throw** | **no-throw** |
| `String[][] <- Integer[]` (array of arrays) | ASE | ASE | **no-throw** |
| `Cat[] <- Dog` (sibling subclasses) | ASE | ASE | **no-throw** |
| `Object[] <- Integer` | no-throw | no-throw | no-throw |
| `String[] <- null` | no-throw | no-throw | no-throw |
| `Animal[] <- Cat` | no-throw | no-throw | no-throw |
| `Number[] <- Integer` | no-throw | no-throw | no-throw |
| `String[][] <- String[]` | no-throw | no-throw | no-throw |
| `String[] <- String` | no-throw | no-throw | no-throw |

Two readings, and they need separating.

**The JIT loses the check for every shape, without exception.** All five
throw-shapes read `no-throw` once compiled. This is not selective and there is
no shape that survives, because the compiled path contains no check to be
selective with — the store is unconditional. The six legal shapes are unaffected
for the same reason, so the fix has **no false-positive risk to remove**: today's
compiled path already accepts everything, and the guard can only ever move a
store from "accepted" to "refused".

**`Animal[] <- String` is wrong in the interpreter too**, and that is a separate
defect this record did not have. It is not an oversight but a deliberate blanket:
`vm/src/runtime/interpreter/typecheck.rs:711` fails open for **every** interface
component, on the stated grounds that dynamic proxies, annotation proxies and
synthetic classes implement interfaces invisibly to the static hierarchy. The
grounds are real; the scope is not. The three escape hatches that actually
handle those cases (`$Proxy`/`AnnotationProxy` by name, `class_chain_reaches_
proxy_instance`, `synthetic_implements`) all sit **below** this early return and
are therefore unreachable for interface components — the blanket makes its own
justification dead code, the same shape of mistake as row 7. Nomination §B11.6-N3.

## §B11.3 — Where the guard goes: one site, and the others are already closed

`jit/src/x64/bytecode_walk.rs`, the `0x53` arm (opens at line 1778, in the
comment block beginning line 1764). The guard belongs **after
`self.emit_bounds_check(pc)` and before `self.emit_ref_aload_regs()`** — that
is, after the null and bounds checks (whose stubs and dataflow elision must be
preserved, and which the helper's own header reads depend on) and before the
SATB pre-write barrier, because on the exception path no element is read and
none is written.

The other three lowerings need nothing, and this was checked rather than assumed:

* `jit/src/ir.rs` — the IR builder's array-store arm is `0x4f | 0x50 | 0x54 | 0x55 | 0x56`. `0x53` is **absent by design**, with the reason stated in place (a reference store needs the SATB and card barriers the IR tier does not emit). An unhandled opcode refuses the method.
* `jit/src/ir_lower.rs:4780` — belt and braces on the same point: `Op::ArrayStore(MemKind::Ref)` latches a `Bailout` rather than emitting a barrier-less store.
* `jit/src/aarch64_backend.rs` — `0x53` is in the unsupported set, and `object_model_opcodes_are_all_unsupported` is a test that fails if anyone implements it without updating the table.

So there is exactly one emission site to change, and two of the three other
backends carry a test that will tell their author to come back here.

**The `has_dispatch` objection is mostly already paid.** Part 4 warned that
wiring the helper "forces `has_dispatch` on nearly every compiled method" via
`emitted_checkcast_throw`. But `has_dispatch` (`jit/src/x64/driver.rs:1901`) is
already true whenever `bounds_check_stubs` or `null_check_store_stubs` is
non-empty — and the `0x53` arm pushes into **both** on the very lines above
where the guard would go (`jit/src/x64/arrays.rs:239` and `:417`). Any method
containing a compiled `aastore` therefore already has `has_dispatch == true`.
The residual exposure is only the method where *both* checks are elided — BCE
put the pc in `bounds_safe_pcs`, and `is_local_nonnull` proved the array
non-null — which is a hot counted loop over a known-non-null array, i.e. exactly
the case the inline cache below is designed for anyway.

## §B11.4 — The inline cache, and what it costs when it hits

The key insight is a layout one, and it makes the cache much cheaper than the
"walk the class hierarchy" framing suggests:

> "The `class_id` on a Reference array holds the **component** class id."
> — `vm/src/runtime/interpreter/typecheck.rs:409`

and `class_id_offset_in_obj` is **0 by contract** (`jit-api/src/lib.rs:2502`),
with `ARRAY_LENGTH_OFFSET = 4`. So for a reference array, the single 8-byte word
at `[array + 0]` holds the component class id in its low half and the array
length in its high half — and the bounds check immediately above already loads
`[RAX + 4]`, so that word is in L1 by construction.

The question `aastore_element_assignable` answers is a pure function of exactly
two `u32`s: the array's component class id and the value's class id. That pair
fits in one 64-bit word, so the cache is **one word and one compare**, not two.

Per-site cache: `#[repr(C)] struct JitAastoreIC { key: AtomicU64 }` — the packed
`(component_class_id << 32) | value_class_id` pair the helper last **accepted**.
Rejects are never cached (a reject must throw every time, and throwing goes to
the helper regardless).

```text
    ; RAX = array, RCX = index, RDX = value — all three already loaded here
    test  rdx, rdx
    jz    .store               ; JVMS: null is always storable, never consult
    mov   r10, imm64(ic)       ; per-site cache address
    mov   r11, [rax]           ; component class id (low 32) | length (high 32)
    shl   r11, 32              ; component id -> high half
    mov   r8d, [rdx]           ; value class id, zero-extended
    or    r11, r8              ; the accepted-pair key
    cmp   r11, [r10]
    jne   .miss
.store:
    <SATB pre-write barrier, MOV [RAX+RCX*8+16], RDX, card mark — unchanged>
    jmp   .done
.miss:
    <helper ABI>  call jit_aastore_check(vm, array, val, ic)
    <sentinel check via self.helpers.dispatch_threw, as Part 4 describes>
    jmp   .store               ; helper updated the key; no throw pending
.done:
```

**Hit-path cost: 9 instructions, ~26 bytes, one taken-never branch.** Both loads
are ordinarily already resident — `[rax]` is the word the bounds check just
touched, and `[rdx]` is the header of an object the caller has just produced.
The `shl`/`or` dependency chain is 2 cycles on top of the load, and it overlaps
the SATB barrier's own work. There is one conditional branch that can mispredict,
not two; that is why the key is packed rather than compared as two `u32`s.

The `mov r10, imm64` can be dropped (10 of the 26 bytes) by placing the cache
word within `±2GB` of the code buffer and using `cmp r11, [rip+disp32]`. Worth
doing only if the code cache and the IC arena are already co-allocated.

Three correctness obligations that are **not** optional:

1. **Invalidation.** The cached verdict is a claim about a class hierarchy that
   redefinition can change. The IC must hang off the same invalidation path
   `JitMICSlot`/`JitPICSlot` already use; a stale accept is a silently wrong
   store, which is the bug we are fixing.
2. **Class-id reuse.** If a `ClassId` is recycled after unload, a stale key can
   match the wrong pair. The MIC/PIC carry identical exposure, so whatever they
   rely on applies — but it must be stated, not inherited by accident.
3. **The key is meaningful only for a `Reference`-element array.** This adds no
   new exposure: today's inline lowering already does an unconditional 8-byte
   reference store and so already assumes exactly that, on the verifier's
   guarantee. The guard assumes strictly less than the code it is guarding.

What the IC does **not** fix is the megamorphic site — a store loop whose value
type genuinely rotates misses every time and pays the full helper. Whether that
needs a 4-way cascade (the `JitPICSlot` shape, already built) is a question for
after the 1-entry form is measured, not before.

## §B11.5 — The A/B, run

Both arms of the real decision cannot be run on one binary: the guarded arm does
not exist without a rebuild, which this lane may not do. So the harness measures
the two things that are on today's binary and that bracket the answer.

* **BASE** — `Object[] <- String`, monomorphic, 2M stores. Exactly today's inline lowering.
* **CHK** — the identical loop with an explicit `(String)` cast before the store. `checkcast` **does** call its Rust helper on this binary, so CHK = BASE + one Rust-boundary type check per reference store. This is the shape of Part 4's naive patch.
* **POLY** — BASE with a 4-way rotating value type, bounding the megamorphic case.

ABBA-interleaved within each repetition, 9 repetitions, first 3 discarded, and
the **minimum** reported: on this shared host the minimum is the only statistic
that is not a measurement of the other tenants. Four independent process
launches:

| run | BASE ns/store | CHK ns/store | CHK/BASE |
|---|---|---|---|
| 1 | 106.4 | 349.1 | **3.28** |
| 2 | 114.2 | 388.8 | **3.40** |
| 3 | 105.0 | 369.3 | **3.52** |
| 4 | 111.6 | 393.0 | **3.52** |
| HotSpot 25 (reference) | 4.9 | 7.0 | 1.43 |

**The number is trustworthy and the caveat is narrow.** The absolute values
drift with host load — within a single run the max/min spread is 1.3x, and under
`--nojit` it is 1.8x — but the *ratio* reproduces to within 7% across four
launches, because ABBA interleaving puts both arms under the same drift. The
per-repetition medians and maxima are noise and are not quoted.

Control that the measurement is a JIT measurement at all: `--nojit` gives BASE
2389 ns/store against 106 ns/store here, a **22x** gap, so the loop is compiled.
CHK is compiled too (349 vs 4034 ns/store interpreted), so CHK is genuinely
"compiled code making a helper call per store" and not a partial-interpretation
artefact.

**CHK is a LOWER bound on Part 4's naive patch, not an upper one.**
`jit_checkcast` takes the class name as a `(ptr, len)` from the site and does one
lookup. `aastore_element_assignable` calls `array_descriptor_of`, which
`format!`s a **fresh `String` on every single store**, then `to_string()`s the
component name, then takes `class_manager.read()` two or three times. Routing
every `aastore` through it would cost more than 3.4x, and would put two heap
allocations on the hottest reference-store path in the VM.

So: **the objection that held Part 4 back was correct, and is now quantified.**
The plain call is not landable on the store path. The 9-instruction cache is.

## §B11.6 — NOMINATIONS

### N1 (literal, `jit/src/x64/bytecode_walk.rs`, in the `0x53` comment block at line 1773) — the premise is falsified, and it is the only thing telling the next reader this is fine

OLD (exact):

```rust
                // ArrayStoreException note: the current `jit_aastore` helper does NOT enforce
                // the ASE check (the interpreter does it via `set_array_element`). This inline
                // path matches the helper's behavior exactly — no regression. Wiring an inline
                // ASE check is a follow-up that needs type-narrowing infrastructure (not yet
                // tracked in this JIT).
```

NEW:

```rust
                // ArrayStoreException: THIS PATH IS NOT JVMS-CONFORMANT. It performs the
                // store unconditionally, so a JIT-compiled `String[] <- Integer` succeeds and
                // leaves an Integer in a slot the verifier proved is a String — a live heap
                // type confusion, not a message-wording difference. Measured 2026-08-12: all
                // five ArrayStoreException shapes read `no-throw` once compiled
                // (W7-37 §B11.2).
                //
                // The note this replaces said the inline path "matches the helper's behavior
                // exactly — no regression". That was true when written and was falsified,
                // silently, when the JVMS §aastore covariance check was later added to
                // `jit_aastore` in a different crate: a premise stated in a comment is not a
                // compile-time link. `jit_aastore` now has the check and has had NO CALLER
                // since R20 / HIGH-5.
                //
                // The fix is not to restore the call — a Rust-boundary type check per
                // reference store measures 3.4x on this VM's own store loop (W7-37 §B11.5).
                // It is the 9-instruction per-site inline cache in W7-37 §B11.4, keyed on the
                // packed (array component class id, value class id) pair. Insert it after the
                // bounds check and before the SATB barrier below.
```

### N2 (design, same arm) — the guard itself

Emit the sequence in §B11.4 between `self.emit_bounds_check(pc)` and
`self.emit_ref_aload_regs()`. Needs: a `JitAastoreIC` per site (mirror
`JitPICSlot`'s `#[repr(C)]` + layout-assertion discipline in `jit/src/lib.rs`,
including its invalidation wiring), a `jit_aastore_check(vm, array, val, ic)`
helper in `vm/src/jit/helpers.rs` that reuses `aastore_element_assignable` and
the `throw_runtime_error` funnel `jit_aastore` already routes through, and the
`self.helpers.dispatch_threw` + `SHL RAX, 63` sentinel mapping Part 4 spells out
for the void return. Part 4's `has_dispatch` note can be relaxed per §B11.3.

Verification is `RExceptions`: all eleven `aastore` shapes must read their MUST
value in both tiers. Do not accept a green that comes from `arrayStoreMessage()`
alone — that is the single shape that hid this for two rounds.

### N3 (literal, `vm/src/runtime/interpreter/typecheck.rs:711`) — the interface blanket buries its own justification

`Animal[] <- String` does not throw on CratonVM in **either** tier. The cause is
a fail-open for every interface component, placed **above** the three escape
hatches that handle the cases it cites, which makes them unreachable for exactly
the component kind they were written for.

OLD (exact):

```rust
        // Component is an INTERFACE → fail open. Proving a value implements an
        // interface is unreliable in this VM (dynamic proxies, annotation
        // proxies, and synthetic classes implement interfaces at runtime / by
        // name, invisibly to the static hierarchy). A genuine ArrayStoreException
        // essentially always involves a concrete-class component (Number[],
        // String[], …); for an interface[] we don't risk a spurious throw.
        if cm.get_class(comp_id).map_or(false, |c| c.is_interface()) {
            return true;
        }
```

NEW:

```rust
        // An INTERFACE component is NOT a reason to fail open on its own. This
        // used to return `true` for every interface component, on the grounds
        // that dynamic proxies, annotation proxies and synthetic classes
        // implement interfaces invisibly to the static hierarchy. Those grounds
        // are real — but each of them already has its own escape hatch BELOW
        // (the `$Proxy`/`AnnotationProxy` name test,
        // `class_chain_reaches_proxy_instance`, `synthetic_implements`), and
        // this early return sat ABOVE them, so for interface components those
        // three could never run. The blanket made its own justification dead
        // code, and cost the whole rule: `Animal[] <- String` did not throw
        // (W7-37 §B11.2). `is_subclass_of` handles interfaces directly
        // (`classloading/src/class.rs`, `is_subclass_of_interface`), so a
        // statically-declared implementor still passes below; a proxy or
        // synthetic still fails open, one hatch further down, on the specific
        // ground that applies to it.
```

i.e. delete the early return and keep the comment as the record of why it is
gone. Verification: `Animal[] <- Cat` and the annotation-proxy `arraycopy` row
already in `RExceptions` must stay green, the `aastore_fails_open_across_a_
split_loaders_two_copies_of_one_name` unit test must stay green, and
`Animal[] <- String` must go red-to-green. If the hibernate-smoke annotation
regression the comment cites reappears, the right answer is to widen the
proxy hatch below, not to restore the blanket.

### N4 — §B8.1 stands; amend its rationale

The `klass_origin` patch in §B8.1 is correct and should land as written. Its
justification needs one word changed: the array branch is not code that "has
never executed", it is code that executes **only when the array class was
already in the definition index for some unrelated reason** (§B11.1). That is
why the row has been recorded as fixed twice — a probe that allocates the array
type it is asking about reports it fixed. Resolving the *component* rather than
the array is exactly what removes the dependence.

## §B11.7 — What landed in `RExceptions.java`, and what it now reads

Added, all verified on HotSpot 25 first: **`PASS RExceptions (58 checks)`**, up
from 25.

* The eleven-shape `aastore` matrix of §B11.2, each store in its own method so each gets its own compiled site, each read cold and hot.
* The four array-operand cast shapes of §B8.2, each read cold and hot.
* The `System.arraycopy` covariance row of §B11.8-4, cold and hot, plus the destination-untouched claim. Green on both VMs today.
* `expect()` / `drainDivergences()` — counted like `check()`, but **records and prints** the divergence instead of throwing at the first one, then throws once at the end with all of them. This is a direct response to how row 7 stayed wrong: a family of assertions that dies on its first member gives a taker one member per rebuild of this VM.

On today's CratonVM binary the fixture prints **12 `DIVERGENCE` lines in a single
run** before dying at the pre-existing row-8 tier-parity `must`, which is
unchanged and still fires. The 12 are: 6 cast rows (3 shapes x 2 tiers; the
`String` → `String[]` pair passes, see §B11.1) and 6 `aastore` rows (5 JIT +
`Animal[] <- String` interpreted).

**Two independent fixes are needed to clear this file**, and a taker should not
expect either one alone to go green:

| divergences | cleared by |
|---|---|
| 5 `aastore` JIT rows + the row-8 `must` | §B11.6-N2 (the guard) |
| 1 `aastore` interpreted row | §B11.6-N3 (the interface blanket) |
| 6 cast rows | §B8.1 (`klass_origin`) |

## §B11.8 — Residuals

1. **The `String` → `String[]` cast row passes in `RExceptions` for an incidental reason** — the fixture's own `new String[1]` static indexes `[Ljava.lang.String;`. It is left in because it is one of §B8's four named shapes and because the pass is itself evidence for §B11.1, but it is not load-bearing: after §B8.1 lands it should pass for the right reason, and the way to confirm that is the `String` → `Long[]` shape (an array class the fixture never instantiates), which reads bare in both tiers today.
2. **The megamorphic `aastore` site is unmeasured against a guard.** POLY was measured on the unguarded binary only (169-192 ms/2M stores, i.e. *faster* than BASE, since it stores into a different array). A 1-entry IC's miss rate on real polymorphic store sites — `ArrayList.add` across a heterogeneous list, `HashMap` resize — is the open question, and the answer decides 1-entry vs the existing 4-way `JitPICSlot` shape. It cannot be answered without the guarded build.
3. **The JIT `checkcast` helper emits a fourth wording** (`class X cannot be cast to class Y`, prefixes present, parenthetical absent) that no part of this record previously described. §B8.1 fixes `klass_origin`, which is the *source* of the missing clause; whether that alone makes the JIT arm byte-identical is untested, because the JIT arm reaches the funnel by a different route. Re-run §B11.1's five-shape cold/hot table after §B8.1 lands, not just §B8's six-case cold table.
4. **Not a residual — closed.** The same JVMS rule has a third implementation, `System.arraycopy` into a covariantly-typed destination, and `RExceptions` previously exercised it only in the always-legal direction. Measured: `arraycopy(Object[]{Integer}, 0, String[], 0, 1)` throws `ArrayStoreException` on CratonVM **in both tiers**, byte-for-byte agreeing with HotSpot, and leaves the destination element `null`. So of the three implementations of the covariance rule, the reflective one (`Array.set`, Part 4) and the bulk one (`arraycopy`) are correct and only the `aastore` opcode is not. A scheduled row for it is now in `RExceptions` — it is green today, and it is there so that a taker implementing §B11.6-N2 cannot fix the opcode by weakening a shared predicate without hearing about it.
