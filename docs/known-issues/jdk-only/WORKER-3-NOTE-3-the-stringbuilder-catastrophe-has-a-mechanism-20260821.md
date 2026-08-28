# WORKER-3-NOTE-3 — the StringBuilder catastrophe, diagnosed to a mechanism, three of its causes fixed, and the one that is a VM project

**Status: N1 CLOSED 2026-08-28. The rest still OPEN — MEASURED throughout.**

> **N1 — the layout migration — is landed.** `sb_store_units` is the writer half
> this note named: the payload that is already there decides the layout, so a
> builder is never converted from one representation to the other and the torn
> object §3 describes cannot be produced. The residual §3 named — the
> `ArrayStoreException` from `arraycopy: can not copy byte[] into char[]` — is
> gone with it, and three further defects nobody had connected to this note fell
> out at the same time: `chars()`, `codePoints()` and `compareTo` read
> `value`/`coder` directly and had been answering a LATIN1 truncation of every
> character above U+00FF, and `writeObject` threw. MEASURED, 747 probe rows,
> 0 differing lines against HotSpot jdk-25.0.4+7 in both modes. See the retired
> `l2-strings-eighteen-defects-five-root-causes-and-the-writer-half` write-up
> and the open `l2-strings-residuals-the-migration-is-unpriced` page.
>
> **N2, N3, the six refusals of §4 and the §7 refusal of the `java.lang.invoke`
> block are UNTOUCHED** and are why this page is still here. Read the body for
> them; read this banner, not the body, for N1's status.

**Original status line: OPEN — MEASURED throughout.** WORKER 3, 2026-08-21, Linux build host,
clean builds of `22cb4338d` (control) and of this branch. Every arm below was
run; nothing here is argued from a grep.

`H14-3` priced `register_string_builder_natives` — **192 registrations, the
largest single registrar in the `java.lang` block and 57% of this lane's owned
surface** — at **zero vectors**. `H22` then measured what that zero concealed:
armed across the three classes the registrar covers, **every `append` was
silently discarded and `toString()` returned the empty string, with no exception
and `rc=0`**, and `StringBuilder` alone was fine, `AbstractStringBuilder` alone
was fine, and the two together broke.

Nobody had a mechanism. This record is the mechanism, and it is not one defect.

---

## 1. The instrument that was missing

The corpus **never asserted the CONTENT of a built string**. That is trap 5 of
the worker briefs in the place it cost the most: a suite that only checks that
`toString()` returned *a* String cannot distinguish "" from "abc1true2xy", so
`H14-3`'s zero was not evidence the code was right — it was evidence nothing was
looking.

`regression-suite/src/RStringBuilderContent.java` is 34 value assertions over
the three classes, and the first thing it did was turn `H22`'s finding from a
silent `rc=0` into

```text
  AssertionError: sb.toString: expected [abc1true2xy] got []
```

which is a sentence somebody can act on.

## 2. Three causes, measured, and what each one was

### 2.1 The reader and the writer disagreed about where `count` lives

`sb_set_count` mirrors the builder length into the field **named** `count`,
because slot 2 is not `count` on a modern image — a 2026-07-26 fix with its own
paragraph in the source. `sb_state`, the **reader**, kept indexing slot 2.

MEASURED, `javap -p --system <image> java.lang.AbstractStringBuilder`:

```text
  JDK 17.0.20          value@0  coder@1  count@2
  JDK 21.0.12          value@0  coder@1  maybeLatin1@2  count@3
  JDK 25.0.4           value@0  coder@1  maybeLatin1@2  count@3
  CratonVM synthetic   value(char[])@0   count@1
```

So on JDK 21 and 25 the reader returned **`maybeLatin1` — a boolean — as the
builder's length**. Both sides now resolve the slot by name through one helper.

A slot COUNT cannot decide it, and that is why the earlier fix reached for
`object_num_fields` and only got half the way: **JDK 17's `StringBuffer` also
has four slots** (`value, coder, count, toStringCache`) and its `count` is at 2.

### 2.2 Growing a real-layout builder DISCARDED its content

`sb_ensure_capacity` allocates a fresh `char[]` and bulk-copies the old payload
into it — under a guard requiring the old payload to be a `char[]`. For a
receiver whose `value` is the real compact `byte[]` the guard is false, and the
`else` arm did not exist: control fell straight through to installing the new,
empty array. **Every character already in the builder was dropped, silently.**

The same function then returned `buf.unwrap()` on a path where `buf` could be
`None`, which is a Rust panic — process death, not a Java throwable — for a
zero-length request on such a receiver. Both are gone.

### 2.3 `toString()` of a real-layout builder returned `""`, and here is exactly where

`javap -p -c --system <jdk-25> java.lang.StringBuilder`:

```text
  public java.lang.String toString();
       1: invokevirtual  Method length:()I
       4: ifne  10
       7: ldc   String ""
      10: new   class java/lang/String
      16: invokespecial Method java/lang/String."<init>":(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V
```

That constructor **is registered as a native**, and its body reads the builder
through `sb_read_chars`, which returned an **empty vector** for a
`byte[]`-backed receiver. So real JDK `toString()` bytecode produced `""` for a
builder holding text — and the function's own doc comment already named the
case it fails on: *"Byte Buddy can execute that real bytecode while
retransformation is in progress."*

MEASURED, dial armed on
`java/lang/StringBuilder,java/lang/AbstractStringBuilder`, a probe that reads
the fields reflectively, before and after:

```text
  HotSpot   len=2  ts.length=2  ts.isEmpty=false  String.value=[B/2
  before    len=0  ts.length=0  ts.isEmpty=true   String.value=[B/0   value=byte[16] coder=0 count=2
  after     len=2  ts.length=2  ts.isEmpty=false  String.value=[B/2
```

**`count` was 2 the whole time.** Three independent readers of one object
disagreed, and each of them answered rather than refusing.

**These three are not dial-only defects.** The dial is how they were *found*.
Mockito's inline mock maker and Byte Buddy retransformation both produce
real-layout builders in ordinary runs, which is what the write-side twin of
§2.1 already cost: `org.springframework.cglib.core.TypeUtils.map` reads
`sb.length()`, got 0, and every cglib proxy generated after the first Mockito
mock in the process died in `CGLIB$STATICHOOK1`. §2.2 and §2.3 are the same
object model failing on the read side and had no such record.

## 3. What is left, named, because a diagnosis without the next failure is not one

With all three fixed, the armed arm gets **56 assertions further** and then:

```text
  ArrayStoreException: arraycopy: type mismatch: can not copy byte[] into char[]
      at RStringBuilderContent.main(RStringBuilderContent.java:76)
      at java/lang/StringBuilder.replace(StringBuilder.java:307)
      at java/lang/AbstractStringBuilder.replace(AbstractStringBuilder.java:1123)
      at java/lang/String.getBytes(String.java:4797)
```

A loud exception with a stack, where there was a silent empty string.

**The residual is the object model, and it is `H25-3` N5's job, not a bug fix.**
CratonVM keeps two incompatible representations of one class:

| | `value` | length |
|---|---|---|
| CratonVM synthetic | `char[]`, one unit per char | `count` @1 |
| real JDK 17/21/25 | compact `byte[]` + `coder` | `count` @2 or @3 |

The natives write the first; real `AbstractStringBuilder` bytecode writes the
second. **Any run in which some calls take one path and some take the other
holds a torn object**, and the enforcement dial produces exactly that mix by
construction — `H17-2` measured that it reaches one dispatch door, so arming a
class moves its *cold, step-1* calls and leaves warm-cache, interceptor,
reflective and JIT-bound calls on the native. `H22`'s "`StringBuilder` alone is
fine, `AbstractStringBuilder` alone is fine, together they break" is that mix,
and it is not a property of either class.

The fix that closes it is the one `H25-3` N5 names: **make the natives operate
on the real compact layout**, i.e. a layout-aware writer to match the
layout-aware reader this branch added (`sb_value_units`), so a builder is never
converted from one representation to the other mid-life. That is ~15 functions
in `lang_string.rs` and it must be priced against the UNARMED path, which is
100% of production and is green today. **This lane did not attempt it, and a
half-migrated layout would be worse than none.**

## 4. Six refusals, with the evidence for each

Each is a row a plan driven by row counts would have deleted.

* **R1 — `java/lang/StringUTF16.isBigEndian()Z`, KEPT.** `H25-3` R3 refused it
  on a comment; this refuses it on a measurement. Declared by all three JDK 17
  images and all three JDK 21 images, absent from JDK 25 — the `PARTIAL` shape,
  now with a nine-image table behind it.
* **R2 — `Thread.stop0`, `suspend0`, `resume0`, `countStackFrames`, KEPT.**
  `H25-1` §1.4 listed all four as "removed from the JDK, not deprecated … stubs
  for an API surface that stopped existing". MEASURED: `stop0`, `suspend0` and
  `resume0` are declared by every JDK 17 image, and `countStackFrames` by JDK 17
  **and** JDK 21. **Four of the six rows that record proposed as dead are live
  on a supported image.**
* **R3 — `AbstractStringBuilder.toString()`, KEPT and marked.** `public
  abstract`, no `Code`, not `ACC_NATIVE`, on all nine images. The registration
  is the only implementation there is. Marked at the registrar per `H25-2` N3.
* **R4 — four `java/lang/invoke` rows, KEPT and marked.**
  `MethodHandle.rebind`, `VarHandle.withInvokeBehavior`,
  `withInvokeExactBehavior`, `accessModeTypeUncached` — abstract on every image,
  same argument as R3. The comment above two of them already said what a
  retirement restores: `AbstractMethodError` in
  `sun.security.provider.SHA3.<clinit>`.
* **R5 — `ClassLoader.getPackages()` stays unconditionally EMPTY.** The new
  `RLangPackages` vector wanted to assert it non-empty and the tree refused,
  with a reason at the site: the real bytecode's stream pipeline leaks a
  `ReferencePipeline$Head` into a `Package[]` local and NPEs
  `org/jboss/modules/ConcurrentClassLoader.<clinit>` on WildFly 39 boot. The
  vector asserts the SHAPE the override owes either way — a `Package[]`, not an
  `Object[]` — and says at the site why it does not assert the count.
  **Asserting `length == 0` would have frozen a divergence into the gate and
  called it correct; asserting `length > 0` would have asked CratonVM to break
  WildFly.**
* **R6 — the `MethodHandle.invoke` / `invokeExact` losers in
  `native-builtins/src/lib.rs`, NOT retired.** They are `owns_slot: false`
  against `register_t4_method_handle_invoke` and are the same landmine as the
  four this lane did delete. They are also two members of a five-method family
  (`invoke`, `invokeExact`, `invokeBasic`, `linkToStatic`, …) served by one
  handler, and this lane did not measure the registrar order for that family on
  the synthetic arm. Nominated rather than guessed at.

## 5. What this does NOT establish

* **The armed StringBuilder arm is NOT green**, and this record does not claim
  the registrar is now safe to arm. It claims three named causes are fixed and
  the next failure has a name (§3).
* **§2's three fixes are verified by the unarmed suite and by the armed probe,
  not by an armed suite run.** No arm of `regression-suite/run.sh` was run with
  `CRATONVM_ENFORCE_NATIVE_SHADOW` set: an armed run of the whole corpus prices
  the dial, not the retirement (`H14-3` §6, `H25-2` §3.1), and this lane retired
  nothing in that registrar.
* **Mockito and Byte Buddy were not run.** §2's production reachability is
  ARGUED from the source comments that name those frameworks and from the
  cglib record the tree already holds. This lane reproduced the defects through
  the dial, which is a different door onto the same object.
* **The layout migration in §3 is a DESIGN, not a measurement.** Nobody has
  priced it, and its risk is entirely on the unarmed path.
* **`register_string_builder_natives` is untouched** — 192 registrations, and
  this lane retired 6 of them (three `repeat(String,int)` near-misses, three
  shadowed `appendCodePoint` duplicates), all of which the dump proves were
  never dispatched.

## 6. NOMINATIONS

* **N1 — the layout migration** (§3). `sb_value_units` is the reader half and is
  landed; the writer half is `sb_write_units`, which must write back in the
  receiver's OWN layout instead of converting it. Whoever takes it should take
  `RStringBuilderContent` armed as the acceptance test and the unarmed
  `SUITE=all` as the regression bound.
* **N2 — `H25-1` §1.4's `java/lang` dead list needs the correction in R2.**
  Four of its six `deprecated_lang.rs` rows are live on a supported image. The
  list is quoted as a work list; it is an upper bound.
* **N3 — retire the `MethodHandle.invoke`/`invokeExact` losers in `lib.rs`**
  (R6), after measuring the five-method family's registrar order on the
  synthetic arm.
* **N4 — `H14-3`'s zero-vector column should be re-derived now.**
  `register_string_builder_natives` was priced at zero vectors when no vector
  asserted string content; there is one now, and any other registrar whose
  price is zero deserves the same question asked of it before the zero is
  quoted. *A registrar with no vector is not a registrar with no risk.*


## 7. `java.lang.invoke` is a CAPABILITY gap, not an adjudication problem — MEASURED

The brief gave this lane `java.lang.invoke` at **56 rows**. Adjudicated against
the image, the 149 `java/lang/invoke/*` registrations in the strict dump split:

| image verdict | rows |
|---|---:|
| real bytecode exists behind it — a *retire* candidate by the standard rule | **93** |
| declared `ACC_NATIVE` — a legitimate §1.5 bridge, not a defect at all | 49 |
| declared, NO `Code` — the only implementation there is | 4 |
| declared nowhere | 3 |

So a third of the block is contract-compliant before anyone touches it. The
question is the 93, and the registrar's comment answers it with an ARGUED claim:
*"the real JDK bytecode for these depends on deep JDK internals
(MethodHandleNatives, DirectMethodHandle) we don't support."*

**Measured, by arming the whole `java/lang/invoke/` prefix** and running the six
corpus vectors that exercise it. Trap 2 makes this a ONE-WAY probe — an armed
failure is real, an armed zero is not — and this is the failing direction:

```text
  RJdkHandles       PASS (331 checks)  ->  FAIL lookupAndInvoke: findStatic invokeExact
                                           FAIL insertArguments: NoSuchMethodError
                                             MethodHandle.bindArgumentI(int, int)
                                           FAIL filterArguments: NoSuchMethodError
                                             MethodHandle.editor()   [LambdaFormEditor]
  RVarHandleAccess  PASS (28)          ->  AssertionError: get boolean: expected true got false
  RJdkLookupIn      PASS (48)          ->  AssertionError: the full-power lookup reaches
                                             its own private method
  RJdkLambdas       PASS (38)          ->  LambdaConversionException, caused by
                                             NoSuchMethodError: MethodType
                                             Class.insertParameterTypes(int, Class[])
  RJdkProxy         PASS (37)          ->  PASS (37)
  RJdkStrict        PASS (365)         ->  PASS (365)
```

Four of six break, and **the failures name the missing machinery rather than a
wrong answer**: `BoundMethodHandle`, `LambdaFormEditor`, `MethodType`. Retiring
any of the 93 hands the call to bytecode that cannot run.

**REFUSED, and this is the one refusal in this lane backed by an execution
rather than by an image.** The `java.lang.invoke` block is not addressable by
adjudication at all: it becomes retirable when `MethodHandleNatives` and the
`LambdaForm` machinery are implemented, and not before. `RJdkProxy` and
`RJdkStrict` surviving the arm is the useful residue — whatever they exercise in
this prefix is already served by real bytecode.
---

## INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-3-NOTE-3` — the `H22` StringBuilder catastrophe has a mechanism: two
  incompatible layouts for one class plus a one-door dial. Three causes fixed
  (count slot, content-losing grow, `sb_read_chars` returning empty); the
  residual is the object-model migration (`H25-3` N5). Six refusals with
  evidence, plus a seventh: `java.lang.invoke`'s 93 shadow rows are a
  CAPABILITY gap — armed, four of six MethodHandle vectors die on
  `NoSuchMethodError` for `BoundMethodHandle` / `LambdaFormEditor` /
  `MethodType` internals. **OPEN.**
