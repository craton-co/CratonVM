# `resolve_step1_native`'s `bytecode_available: false` — attempted 2026-08-05, reverted, with numbers

**Status:** OPEN. The hole is real; the obvious fix is measured and **does not
work**. This record exists so the next attempt starts after the failure rather
than at it.

## The hole

`resolve_step1_native` (`vm/src/runtime/interpreter/native_override.rs`) passes
a hard-coded `false` for `resolve_native_dispatch_wave1`'s `bytecode_available`
argument. With `false`, a plain `Bridge` native wins against real bytecode under
`--jdk-only` — which is exactly what contract §1.4 exists to prevent.

## What is cheaper than the L9 doc suggests

L9 says closing this "sends **4,796** shadowing `Bridge` registrations to the
bytecode under `--jdk-only` at once". Two corrections, both measured:

* **4,796 is a static count**, not a dispatch count. It is the number of
  registrations whose *image* declares bytecode. What actually changes is the
  number of triples a given run dispatches, which is far smaller: on the
  `StringPolicyMatrixProbe` workload the census's `native-shadows-bytecode`
  observations went **10 → 24**. On a one-line probe, **0 → 1**.
* **The default `--real-jdk` path costs nothing.** `bytecode_available` feeds
  exactly two branches in `resolve_native_dispatch_wave1` and both are
  `policy.is_jdk_only()`-gated, so the value can be computed only under strict
  mode. Verified, not argued: with the change in, the 392-case matrix in default
  mode was **byte-identical** to the build without it.

The "step 1 must not touch the class manager" objection in the old comment is
also weaker than it reads: `resolve_id(...)?` has already returned for every
triple with no registered native, so the lookup never runs on the common path.

## Why it still fails

Computing `has_code` from the **access flags** — `Code` is present iff the
method is neither `native` nor `abstract`, which is what
`--dump-native-registry`'s `real_declaring_method.has_code` column does — is not
sufficient at a dispatch site. Under `--jdk-only` the VM then dies on:

```text
java/lang/AbstractMethodError: method
  java/nio/charset/CharsetDecoder.decodeLoop(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;)Ljava/nio/charset/CoderResult;
  has no Code attribute
    at java/nio/charset/CharsetDecoder.decode(CharsetDecoder.java:587)
    at java/lang/String.decodeWithDecoder(String.java:1249)
```

Reproduce with `probes/StringAsciiDecodeProbe` under `--jdk-only`; it prints
nothing instead of its four lines.

`decodeLoop` is **abstract** on `CharsetDecoder` and concrete on each subclass.
"Does the real class declare bytecode for this triple?" and "does the method
this call will actually run have a `Code` attribute?" are different questions
the moment hierarchy lookup is involved, and step 1 — which runs *before*
method resolution — only has the class NAME, not the resolved method. So it
cannot answer the second question, which is the one that matters.

Two other regressions on the same run, for completeness:

* `List.of("a").get(3)` message went from `"Index: 3 Size: 1"` to `" Size: 1"`.
* `StringOobMessageProbe` and `StringRegexErrorProbe` lost their first rows.

## What a real fix probably needs

Step 1's whole point is to answer "is there a native?" without resolving the
method. `bytecode_available` cannot be answered honestly at that point. So
either:

1. **Move the §1.4 decision to a site that has resolved the method.** Step 6
   already passes a real `true`; the gap is the triples that never reach step 6
   because step 1 answered first. That is a restructuring of the dispatch order,
   not a one-line change.
2. **Resolve lazily inside the `Bridge` arm only.** The kind is known before the
   flag is used, and `Bridge` is the only kind that consults it. Resolving the
   method there costs nothing for `Intrinsic`/`SyntheticStub` and nothing in
   `Compatible` mode — but it must resolve the way the *invoke* will, walking
   the hierarchy, not `find_method` on the named class.

Option 2 is the smaller change and matches where the cost is affordable. Either
way the acceptance test is the one that failed here: `--jdk-only` must still run
`probes/StringAsciiDecodeProbe`, and the default-mode matrix must stay
byte-identical.
