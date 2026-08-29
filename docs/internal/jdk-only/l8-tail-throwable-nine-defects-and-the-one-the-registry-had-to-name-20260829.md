# L8 tail — `java.lang.Throwable` and the exception hierarchy: nine defects, and the one the registry had to name

**Status: CLOSED.** 2026-08-29, branch `claude/l8-tail-20260829`, worktree
`/data/cvm-l2s-20260828` on the Linux build host. Oracle: HotSpot
`jdk-25.0.4+7`, the same image CratonVM ran against.

Second batch of the long tail, and the largest single group in it: **117
bridge-with-code rows across 42 classes**, of which `Throwable` itself is 16 and
the other 41 are two to five each.

`apps/probes/ThrowableFamilySweep.java`, **3975 rows, 0 differing lines against
HotSpot in both `--jdk-only` and compatible mode**, from 296.

---

## 0. Why 42 classes are one batch

They are one REGISTRATION SET. `register_throwable_subclass_natives` walks
`THROWABLE_FAMILY_CLASSES` and gives every entry the same treatment: the
constructor shapes the real class declares, then `getMessage`,
`getLocalizedMessage`, the three `printStackTrace` overloads, `toString`,
`getCause`, `initCause`, `addSuppressed`, `getSuppressed`, `getStackTrace`,
`setStackTrace`, `fillInStackTrace`. A defect in the shared code is a defect in
all 42, and the cheapest way to find out which is to ask `Throwable`'s questions
forty-two times.

That is what made the probe's arithmetic work: **eight of the nine defects were
found by rows that exist once in the source and 42 times in the output.**

### What is and is not comparable

A stack trace's CONTENT is not a cross-VM invariant — frame lists, line numbers
and the internal frames above `main` all differ legitimately — so every
stack-trace question here is one the specification actually fixes:

* the top frame of a trace captured in a named method is that method, in this
  class, in this file: `getClassName`, `getMethodName`, `getFileName` are asked,
  `getLineNumber` is **not**;
* `printStackTrace` is compared on its NON-INDENTED lines only — the header, the
  `Caused by:` chain and the `Suppressed:` markers — with the `\tat ` frames and
  the `\t... N more` elisions dropped;
* a trace's length is asked as `> 0`, never as a number.

Everything else — messages, cause chains, suppression, the `toString` format,
the refusals `initCause` and `addSuppressed` owe, `StackTraceElement`'s value
semantics, serialization round trips, and the messages of exceptions the VM
itself raises — is exact-value comparable and is compared exactly.

---

## 1. The nine defects

| # | what | rows |
| --- | --- | ---: |
| D1 | `ExceptionInInitializerError(Throwable)` took a message from `cause.toString()` | 8 |
| D2 | `printStackTrace` read the `cause` FIELD instead of dispatching `getCause()` | 3 |
| D3 | `getSuppressed()` handed back its own storage | 1 |
| D4 | `setStackTrace` stored the caller's array without copying it | 1 |
| D5 | `setStackTrace(null)` was a no-op where the JDK throws | 2 |
| D6 | `setStackTrace` with a null element was a no-op where the JDK throws | 2 |
| D7 | `suppressedExceptions` held an `Object[]` in a field declared `java.util.List` | 3 |
| D8 | five constructors wrote the `cause == this` sentinel where the JDK nulls it | 5 |

### D7 — the one only real bytecode could see

`Throwable.suppressedExceptions` is declared `Ljava/util/List;`. The natives
wrote a raw `Object[]` into it. Nothing inside this VM minds, because nothing
here reads a field's descriptor. `java.io.ObjectInputStream` does:

```text
java.lang.ClassCastException: cannot assign instance of [Ljava.lang.Object; to
field java.lang.Throwable.suppressedExceptions of type java.util.List in
instance of java.lang.IllegalStateException
    at java.io.ObjectStreamClass$FieldReflector.setObjFieldValues
    ...
    at java.lang.Throwable.readObject(Throwable.java:941)
```

**Every throwable carrying a suppressed exception failed to deserialize** — and
every try-with-resources whose body and whose `close()` both throw produces one.

The divergence was not hidden. `throwable_suppressed`'s own doc comment said it:
"the JDK sentinel is a List, while CratonVM's native `addSuppressed` replaces it
with a Throwable array; only the latter represents user-visible suppressed
exceptions." What was missing was the price, and the price is a whole
serialization path. **A comment that records a deviation is not a measurement of
it**, and an unpriced one is load-bearing by default.

The fix stores a real `java.util.ArrayList` where one exists and keeps the array
where one does not (a synthetic-JDK image), with a reader that accepts both —
the same self-checking shape `sb_view` uses for the two `StringBuilder` layouts.
The elements are staged into a plain `Object[]` first: filling it runs no
bytecode, so nothing can move while it is built, and one pinned array then keeps
every element reachable across the `add` calls, which do run bytecode.

### D8 — and the door that was not the one being edited

Three of the 62 classes' no-arg constructors chain to a cause-NULLING super, so a
later `initCause` on such an instance is an `IllegalStateException`:

```text
ClassNotFoundException()       aconst_null; checkcast Throwable;
                               invokespecial ReflectiveOperationException.<init>(Throwable)
InvocationTargetException()    the same, then target = null
ExceptionInInitializerError()  super(); aconst_null; invokevirtual initCause
```

Every other class reaches `Throwable()`, which assigns `cause = this`. `Error()`
and `Throwable()` contain an `aconst_null` of their own — the null message
argument to `ThrowableTracer.trace*` under the `jfrTracing` flag — which is why
the discriminator has to be the SUPER CALL and not the presence of a null. The
`(String)` forms of the first two are `super(s, null)` and owe the same refusal:
five constructors in all.

The arm went into the measured per-class constructor table, the build came back,
and `ExceptionInInitializerError(Throwable)` was fixed while `()` and `(String)`
were **unchanged**. `--dump-native-registry` says why in one column:

```text
ExceptionInInitializerError ()V                      lang_misc.rs:3390  owns_slot=False inv=0
ExceptionInInitializerError ()V                      lib.rs:11197       owns_slot=True  inv=2
ExceptionInInitializerError (Ljava/lang/String;)V    lang_misc.rs:3390  owns_slot=False inv=0
ExceptionInInitializerError (Ljava/lang/String;)V    lib.rs:11203       owns_slot=True  inv=4
ExceptionInInitializerError (Ljava/lang/Throwable;)V lang_misc.rs:3390  owns_slot=True  inv=10
```

A second registrar in `lib.rs` owns two of the three descriptors; the table's
rows for them are inert. The `(Throwable)` row has no duplicate, which is exactly
why that one fix landed. (`InvocationTargetException`'s two cause-bearing
constructors are the same shape again, owned by a third registrar in
`reflect_annotations.rs` — already correct, but only by coincidence of it having
been written there.)

**A fix that works for one descriptor and not its neighbour is the signature of a
method with more than one registrar**, and `owns_slot` is the column that settles
it — not the source, and not which registrar looks canonical.

### D2 — and its own sibling, twenty lines up

`printStackTrace` walked the chain by reading the `cause` field. Several
subclasses override `getCause()` to answer a field of their own —
`InvocationTargetException` returns `target`, and this VM's registrar has an
entry saying precisely that — so a wrapped exception printed its header and
nothing else:

```text
new InvocationTargetException(new IOException("io"), "wrapped")
  HotSpot   java.lang.reflect.InvocationTargetException: wrapped
            Caused by: java.io.IOException: io
  was       java.lang.reflect.InvocationTargetException: wrapped
```

The correction was already written for the other half of the same header:
`throwable_to_string_text` dispatches `getLocalizedMessage()` rather than reading
`detailMessage`, and its doc comment gives the reason. One half of a printed
header consulted the receiver's implementation and the other half did not.

### D1 — a class-specific constructor routed through the generic arm

`ExceptionInInitializerError(Throwable)` is `aconst_null; aload_1; invokespecial
LinkageError.<init>(String,Throwable)` — an explicit null message. The generic
`(Ljava/lang/Throwable;)V` body derives `detailMessage` from `cause.toString()`,
which is right for `Throwable(Throwable)` and wrong here:

```text
new ExceptionInInitializerError(new IllegalStateException("seed")).toString()
  HotSpot   java.lang.ExceptionInInitializerError
  was       java.lang.ExceptionInInitializerError: java.lang.IllegalStateException: seed
```

There is no `exception` FIELD to write on a modern image, which is worth saying
because the older shape is what one expects: `getException()` is
`return super.getCause()`, and `readObject` maps the serialized `exception` name
onto the cause. This was only ever about which super constructor runs.

### D3–D6 — the defensive copies and the refusals

`getSuppressed()` returned the stored array itself, so a caller writing into the
array it was given edited the throwable. `setStackTrace` stored the caller's
array reference with no clone and no validation, so all three of its rules were
wrong at once. The JDK's own bytecode is unambiguous — `aload_1; invokevirtual
clone; checkcast`, then a scan throwing `NullPointerException("stackTrace[" + i +
"]")`, both BEFORE the monitor — so both refusals are owed even by a throwable
whose stack trace is not writable.

---

## 2. What PASSED

Nothing about these classes is local: `Throwable` is on the path of every
`catch`, and `addSuppressed` is on the path of every try-with-resources. So the
change was measured against **every probe in the tree**, both modes — 43 probes,
and the four that ask about suppression directly are named first:

```text
IoSystemSweep              154 rows   0-diff   <- setStackTrace / getStackTrace
Phase3Sweep                 35        0-diff
JdkOnlyBreadthProbe         16        0-diff
StringBuilderShadowSweep   747        0-diff
UriRecompositionSweep     1258        0-diff   <- the batch before this one
OpaqueUriProbe             648        0-diff
InetFamilySweep            468        0-diff
L4TypedBufferSweep         512        0-diff
L4FileSweep                488        0-diff
L4FilesSweep               397        0-diff
L4ByteBufferSweep          406        0-diff
MapViewsShadowSweep        302        0-diff (compat)
UriRawAccessorSweepProbe   245        0-diff
TreeShadowSweep            234        pre-existing (see below)
L4StreamTailSweep          210        pre-existing
PropertiesShadowSweep      184        0-diff
CollectionsShadowSweep     174        0-diff
DequeListShadowSweep       174        pre-existing
ArrayListShadowSweep       166        0-diff
UtilTailShadowSweep        148        0-diff
HashtableVectorShadowSweep 138        0-diff
L4PrintStreamSweep         125        0-diff
LocaleDateTzShadowSweep    125        0-diff
PqOptionalShadowSweep      127        pre-existing
TailFamilySweep            117        0-diff
LangMiscSweep              117        0-diff
LinkedSequencedShadowSweep 104        pre-existing
Phase1Sweep                 80        pre-existing
CharacterSweep             275        0-diff
MathSurfaceSweep         41195        pre-existing
UriLocaleSweep             403        the 18 recorded Locale display-name rows
... and eleven more small ones
```

**129 differing lines survive across all 43 probes, and not one of them mentions
`suppress`, `stackTrace`, `getCause`, `Throwable` or `printStack`.** They are
other lanes' open items, each recognisable on sight: the collections lane's
fail-fast `ConcurrentModificationException` gap (`no-throw` where HotSpot
throws), the FFM lane's `Arena`/`MemorySegment` implementation-class names, a
`Properties` keySet iterator class, `p12=false`, `skip at EOF`, a reversed live
view. That negative is the argument for this batch's blast radius, and it is a
content argument rather than a count.

### Gates and arms

```text
cargo test -p cratonvm-types                                        RC=0
cargo test -p cratonvm-native-builtins  (the seven gate tests)      RC=0
    stub_ratchet, registrar_drift, registrar_reachability,
    essential_wiring_ratchet, duplicate_registration_gate,
    shim_inheritance_guard, registry_contracts
    ... the same three with --features management                   RC=0
cargo test -p cratonvm-native-builtins --lib                        RC=0
cargo test -p cratonvm-vm --lib                                     RC=0

ARM 2  SUITE=all                        114 of 114 passed
ARM 1  CRATONVM_ARGS=--jdk-only         113 of 114   } RSslEndpointIdentification
ARM 3  SUITE=core                        73 of  74   } only
```

`RSslEndpointIdentification` is dev's own new vector, landed into this branch by
the merge, and it is **flaky on a loaded host in a way that is not attributable
to any VM change**: the harness itself reports

```text
HARNESS ERROR [G4] RSslEndpointIdentification: the HotSpot ORACLE run FAILED (rc=1)
  ... CK RSslEndpointIdentification FAILED java.lang.AssertionError:
      application data must actually flow once the handshake succeeds; got: []
```

CratonVM passed all four of its checks in the same run. Three consecutive
`--jdk-only` runs of the same binary gave **green, red, green**, and the red one
failed on the oracle side again. A change to this VM's `Throwable` cannot make
HotSpot's loopback TLS handshake time out. Recorded here rather than fixed
because the vector is not this lane's.

---

## 3. The 280 rows the probe should not have asked

The first run reported 296 differing rows. **280 of them were one bad question of
mine**, repeated across 42 classes and 7 constructor shapes:

```java
p(tag + " suppressed is fresh array", () -> t.getSuppressed() != t.getSuppressed());
```

HotSpot answers `false` for a throwable with nothing suppressed, because
`getSuppressed()` returns the shared constant `EMPTY_THROWABLE_ARRAY`. That is an
implementation constant, not a specification, and CratonVM allocating a fresh
empty array is not a defect. The copy rule only BINDS on a non-empty list — which
is where the probe's `suppressed array is a copy` row asks it, and where it found
D3.

Worth recording because of how close it came to burying the batch: **a row that
is cheap to write is also cheap to write 280 times, and a wrong one at that
multiplicity reads as a systemic defect rather than as a probe bug.** The tell
was structural — the 280 sat in a single `core()` helper, and the ten interesting
rows did not.

The replacement asks something the specification does fix, at the same
multiplicity: the depth of the cause chain each constructor leaves behind. That
row plus three new `initCause afterwards` rows per class are what found D8.

---

## 4. What this does NOT establish

* **No performance measurement was taken.** `addSuppressed` now runs bytecode
  (`ArrayList.<init>` and `add`) where it previously grew an array in Rust. It is
  on the try-with-resources failure path rather than in a hot loop, and a failing
  `close()` is already the expensive case — but that is an argument, not a
  number, and this campaign has been wrong about exactly that before (see
  `l2-strings-residuals-the-migration-is-unpriced-20260828.md`).
* **The array fallback is exercised only by the gate suite.** The
  `--synthetic-jdk` gates pass; no synthetic-mode probe asks about suppression,
  so the fallback branch is asserted by construction rather than measured.
* **`Throwable.computeFormat` is in the 117 and is not reachable from Java.** It
  is not covered by these 3975 rows.
* **The remaining tail is unchanged by this batch**: `java/math/BigInteger` (24
  rows), `java/lang/System` + `Runtime` + `Object` (26), `java/security` (20),
  `jdk/internal` VM/SharedSecrets/Signal (~19), `java/lang/ref` (~12).
