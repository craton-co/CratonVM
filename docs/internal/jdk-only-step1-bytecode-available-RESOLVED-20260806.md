# `resolve_step1_native`'s `bytecode_available` — RESOLVED 2026-08-06

**Status:** RESOLVED. Filed OPEN 2026-08-05 after the obvious fix was measured
and failed. It proposed two restructurings and named an acceptance test.
**Proposal 2 has now been implemented exactly as this record specified it, and
measured — it fails too, and the measurement says the lever was wrong.** What
the hole actually cost is closed; what it appeared to cost is not a dispatch
problem at all.

If you are here to decide whether to try again, read *The lever was wrong*
first.

## The hole, as filed

`resolve_step1_native` (`vm/src/runtime/interpreter/native_override.rs`) passed
a hard-coded `false` for `resolve_native_dispatch_wave1`'s `bytecode_available`
argument. With `false`, a plain `Bridge` native wins against real bytecode under
`--jdk-only` — which is what contract §1.4 exists to prevent — and step 1
answers first for very nearly every dispatch in the VM.

Two corrections the original record already made, both still true:

* **4,796 is a static count** of registrations whose image declares bytecode,
  not a dispatch count. On the `StringPolicyMatrixProbe` workload the census's
  `native-shadows-bytecode` observations went 10 → 24; on a one-line probe,
  0 → 1.
* **The default `--real-jdk` path costs nothing.** Every consumer of the flag is
  `policy.is_jdk_only()`-gated, so the value need only be computed under strict
  mode. Still true of what landed: `--real-jdk` pays one `Copy` field read and
  one enum comparison, and nothing else.

The "step 1 must not touch the class manager" objection in the old comment is
also weaker than it reads: `resolve_id(...)?` has already returned for every
triple with no registered native, so the lookup never runs on the common path.

## Attempt 1, 2026-08-05 — access flags

Computing `has_code` from the **access flags** of the named class — `Code` is
present iff the method is neither `native` nor `abstract`, which is what
`--dump-native-registry`'s `real_declaring_method.has_code` column reports —
died under `--jdk-only` on:

```text
java/lang/AbstractMethodError: method
  java/nio/charset/CharsetDecoder.decodeLoop(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;)Ljava/nio/charset/CoderResult;
  has no Code attribute
    at java/nio/charset/CharsetDecoder.decode(CharsetDecoder.java:587)
    at java/lang/String.decodeWithDecoder(String.java:1249)
```

This record's diagnosis at the time: `decodeLoop` is abstract on
`CharsetDecoder` and concrete on each subclass, so "does the real class declare
bytecode for this triple?" and "does the method this call will actually run have
a `Code` attribute?" are different questions once a hierarchy is involved, and
step 1 could only answer the first.

Two other regressions were recorded on the same run: `List.of("a").get(3)`'s
message went from `"Index: 3 Size: 1"` to `" Size: 1"`, and
`StringOobMessageProbe` / `StringRegexErrorProbe` lost their first rows.

## Attempt 2, 2026-08-06 — proposal 2, implemented as written

> **Resolve lazily inside the `Bridge` arm only.** The kind is known before the
> flag is used, and `Bridge` is the only kind that consults it. […] it must
> resolve the way the *invoke* will, walking the hierarchy, not `find_method`
> on the named class.

That is what `step1_dispatch_has_code` does, and it is what landed:
`crate::classloading::find_method_recursive` from the call site's own dispatch
class (`dispatch_class_override`, else `get_loaded_class_id(class_name)` — the
identical start the site's own superclass walk uses), then `method.code()`.
That IS the interpreter's own resolution: superclass chain first, preferring a
non-abstract match, then interface defaults, then an abstract declaration last.
It never loads a class; an unloaded class answers `false`.

**It produces the identical `AbstractMethodError`, on the same probe.** And the
diagnosis above was wrong about why. `probes/CsShape` — six lines, run against a
HotSpot control:

| | HotSpot 25 | `cratonvm --jdk-only` |
|---|---|---|
| `Charset.forName("US-ASCII").getClass()` | `sun.nio.cs.US_ASCII` | **`java.nio.charset.Charset`** |
| `.newDecoder().getClass()` | `sun.nio.cs.US_ASCII$Decoder` | **`java.nio.charset.CharsetDecoder`** |
| `Charset.defaultCharset().getClass()` | `sun.nio.cs.UTF_8` | **`java.nio.charset.Charset`** |

Both CratonVM answers name an **abstract** class. HotSpot cannot produce an
instance of one; CratonVM's `Charset.forName` intrinsic and `Charset.newDecoder`
bridge do, and every decoder method is then serviced by a native. So
`decodeLoop` resolves *correctly* — to `CharsetDecoder`'s abstract declaration,
because that genuinely is the receiver's class. Sending `decode` to real
bytecode hands the real body an object it cannot service.

The failure is not "step 1 resolved the wrong method". It is "the object was
never real", and no amount of resolution accuracy at step 1 can see that.

## The blast radius — the number this record was missing

`--jdk-only` regression corpus, Azure Linux, Temurin 25.0.3, same binary, one
env var apart:

| | passed | failed |
|---|---:|---:|
| step 1 keeps the bridge (default) | **32** | 17 |
| step 1 yields to bytecode | **3** | 46 |

The 29 newly-failing classes are not dispatch faults. Five families cover them,
each one an instance of the standing "native-backed state is invisible to real
JDK bytecode" defect:

| what real bytecode read | what it got |
|---|---|
| `System.props`, via `System.getProperty` | `null` — the properties live in a Rust side table |
| `Charset` / `CharsetDecoder` instances | instances of the ABSTRACT class, per the table above |
| `String`'s `coder` against its `value[]` | `ArrayStoreException: can not copy char[] into byte[]` |
| `SharedSecrets.javaLangAccess` | `null`, so `ConstantUtils.<clinit>` NPEs and takes `java.lang.constant` with it |
| `TreeMap` ordering | `AssertionError: TreeMap reverse order` |

Under `--jdk-only` **the surviving bridges ARE the object model** for large
parts of `java.base`.

## The lever was wrong

§1.4's remedy for a native standing where bytecode exists is not to yield at
dispatch. It is to **not register the native** — which is where this tree
already puts the decision. `NativeMethodRegistry::register`'s real-JDK drop for
`java/lang/String` says so in as many words:

> Registration was always the real gate; this is that gate, stated once, for
> every dispatch path.

The contract agrees: `NativeShadowsBytecode` is a **recorded** violation, and
`SyntheticStub` is "the only kind rejected under `JdkOnly`". A `Bridge` that
survives registration under `--jdk-only` is, today, one the VM still needs.
Retiring it is per-class, reviewable, and is wave-2 item 4 (the migration
itself). Proposal 1 — move the decision to a site with a resolved method — is
not a way around this: it meets the same objects one frame later.

## What landed

1. **`step1_dispatch_has_code`** — proposal 2's resolution, correct and lazy.
   Under `--jdk-only` only, for a triple that already has a `Bridge`
   registration only (`resolve_id(..)?` has returned for every other triple
   before the walk is even considered), and — while it is only feeding the
   census — at most once per triple, since the sink dedups and a second walk
   buys nothing.

2. **The observation is now unconditional, and that is what the hole really
   cost.** Step 1 records a `NativeShadowsBytecode` row the moment a `Bridge`
   dispatches in front of real bytes, tagged `bridge-ran-over-bytecode` to keep
   it distinct from the existing rows, which mean the opposite (bytecode won).
   `refusals.interpreter_shadow_unenforced` rides in the report beside
   `interpreter_bytecode_preferred`. Before this, step 1 answered first for
   nearly every dispatch and recorded **nothing** — which is exactly L9's
   complaint that "the lists could be inert while the natives kept winning".

   `probes/JdkOnlyBreadthProbe` under `--jdk-only`, one run:

   ```json
   "interpreter_bytecode_preferred": 434,
   "interpreter_shadow_unenforced": 216
   ```

   with **196** distinct `bridge-ran-over-bytecode` rows in `violations[]`
   beside 60 pre-existing `bridge` ones. 196 + 60 = 256 is the sink's cap
   exactly, so **both are floors on this workload** — deliberately: discovering
   a shadow costs a hierarchy walk, and the walk stops once a triple is recorded
   and stops entirely once the sink saturates, because a saturated sink cannot
   learn a new identity. The identities are what item 4 needs. The magnitude
   only has to be non-zero, and from step 1 it was exactly zero before.

3. **The enforcement is a dial, off by default:
   `CRATONVM_JDK_ONLY_ENFORCE_SHADOW=1`.** It exists so item 4 can re-take the
   32/17-vs-3/46 measurement one subsystem at a time instead of arguing about it
   from a grep. Off, `resolve_native_dispatch_wave1` is passed `false` exactly as
   before, so the dispatch decision is byte-for-byte what it was.

## Acceptance test — the one this record named, met

* `probes/StringAsciiDecodeProbe` under `--jdk-only` prints its four lines and
  exits 0.
* `--jdk-only` regression corpus: 33 passed / 16 failed, against 32 / 17 before.
  The single move is `RSerial`, recovered by the `System.Logger` fix landed
  alongside this; the failure list is otherwise identical.
* Default mode is untouched — `--real-jdk` cannot observe that any of this
  exists, and the corpus is unchanged in that mode.
* `scripts/jdk-only-strict-probes.sh`: **PASS**, with three baselined sections
  reported as no longer diverging.

## If you pick this up again

Do not re-attempt the dispatch-order restructuring. Take `--jdk-only-report`'s
`bridge-ran-over-bytecode` rows from the workload you care about, pick the
subsystem with the most of them whose object model is already real, drop those
registrations at `register` time, and re-run the corpus with
`CRATONVM_JDK_ONLY_ENFORCE_SHADOW=1` to watch the number move. The charset
family is the loudest and is blocked on `Charset.forName` returning a real
`sun.nio.cs.*` — its own wave, and `probes/CsShape` is its oracle.
