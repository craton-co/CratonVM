# L8 tail — `java.lang.Throwable` and the exception hierarchy: nine defects, and the one the registry had to name

**Status: CLOSED.** 2026-08-29, branch `claude/l8-tail-20260829`, worktree
`/data/cvm-l2s-20260828` on the Linux build host. Oracle: HotSpot
`jdk-25.0.4+7`, the same image CratonVM ran against.

Second batch of the long tail, and the largest single group in it: **117
bridge-with-code rows across 42 classes**, of which `Throwable` itself is 16 and
the other 41 are two to five each.

`apps/probes/ThrowableFamilySweep.java`, **3973 rows, 0 differing lines against
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
and the four that ask about suppression directly are named first.

**These are `tail-run.sh`'s LINE counts, not row counts** -- each probe prints
two trailer lines (`rows N`, `DONE <name>`), so every figure here is its row
count plus two. Left as the runner printed them rather than adjusted one by one:

```text
IoSystemSweep              154 lines  0-diff   <- setStackTrace / getStackTrace
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

Final state, on the merged tree (`origin/dev` of 2026-08-29 15:00):

```text
cargo test -p cratonvm-types                                        RC=0
cargo test -p cratonvm-native-builtins  (the seven gate tests)      RC=0
    stub_ratchet, registrar_drift, registrar_reachability,
    essential_wiring_ratchet, duplicate_registration_gate,
    shim_inheritance_guard, registry_contracts
    ... the same three with --features management                   RC=0
cargo test -p cratonvm-vm --lib                                     RC=0
cargo test -p cratonvm-native-builtins --lib                         RC=0
    (RC=101 on the tree an hour earlier, on `dev`'s own
     properties_sidetable source-witness guard; `c5f66112d` fixed it)

ARM 1  CRATONVM_ARGS=--jdk-only        115 of 115 passed
ARM 2  SUITE=all                       115 of 115 passed
ARM 3  SUITE=core                       75 of  75 passed
```

The one red seen along the way was `dev`'s own: `5a6348d28 fix(util):
Properties.clone() and replaceAll() NPE` added two functions reading the
unordered side-table snapshot, which that file's own source-witness test
forbids. It was attributed in a single `git diff` rather than bisected -- the
test's input is `include_str!("properties_sidetable.rs")` and nothing else, and
that file was byte-identical to `origin/dev`'s -- recorded in the scope page for
every lane, and left to the lane that wrote it, which fixed it the same hour
(`c5f66112d`).

### A correction, kept rather than quietly edited

An earlier run of the arms was red on `RSslEndpointIdentification`, and **this
page and this batch's commit message both said that was host-load flakiness**:
the harness reported the HotSpot ORACLE failing (`rc=1`, "application data must
actually flow once the handshake succeeds; got: []") while CratonVM passed all
four of its own checks, and three solo runs of one binary gave green, red,
green. That is what the evidence in hand supported, and it was the wrong
conclusion.

`HANDOFF-20260828-SCOPE.md`'s own Vectors section already had the answer, and it
is better than the flake reading: **the vector's client loop threw away the
reply it asserts on.** `unwrap()` consumes one TLS record per call, and under
load the server's NewSessionTicket, its reply and its close_notify arrive in a
single read. Fixed on `dev` the same day, which is why the arms above are green.

Worth keeping because the failure mode is the campaign's most expensive one: a
red that moves with load looks exactly like a flake, and "not mine, and it
moves" is a comfortable enough answer to stop at. **The cheap check that was
skipped was reading the page the vector is documented on before writing a
diagnosis of it.**

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
  is not covered by these 3973 rows.
* **The remaining tail is unchanged by this batch**: `java/math/BigInteger` (24
  rows), `java/lang/System` + `Runtime` + `Object` (26), `java/security` (20),
  `jdk/internal` VM/SharedSecrets/Signal (~19), `java/lang/ref` (~12).
