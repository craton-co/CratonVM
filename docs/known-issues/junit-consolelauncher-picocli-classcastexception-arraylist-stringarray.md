# JUnit ConsoleLauncher bootstrap: ClassCastException ArrayList → String[] (deterministic)

**Status:** OPEN — dev regression in `(8cdc1c011..c3c2b9ee2]`, 2026-07-10.
**Found by:** perf/throughput-20260710 session while re-measuring commons-math
DfpTest; NOT caused by that branch (fails identically with all three of its
feature gates disabled: `CRATONVM_JIT_GETFIELD_HELPER=1`,
`CRATONVM_JIT_INLINE_SELF_GUARD=0`, `CRATONVM_OSR_NEWARRAY=0`).

## Symptom

Every `org.junit.platform.console.ConsoleLauncher` invocation dies during
picocli option parsing, before any test runs:

```
[GC-ARRAY-GUARD] array_length(non-array): kind_byte=0 class_id=63 elem_byte=0
  stored_len=40 obj=0x1aa3de88
Exception in thread "main" java/lang/ClassCastException:
  java.util.ArrayList cannot be cast to [Ljava.lang.String;
    at ...picocli/CommandLine$Model$TypedMember.getToString
    at java/lang/reflect/Field.toGenericString(Field.java:377)
    at java/lang/reflect/Modifier.toString(Modifier.java:259)
    at java/util/StringJoiner.add(StringJoiner.java:191)
```

This blocks the whole commons-math ConsoleLauncher suite (and any other
JUnit-Platform console run).

## Bisection

* `0975a07ad` (BC precipher binary, branched from `8cdc1c011`): DfpTest
  **78/78 pass**.
* dev @ `c3c2b9ee2` (via perf branch merge, feature gates off): **fails
  deterministically**.
* Non-merge candidates in the window: `686de27c1` (StreamDecoder/StreamEncoder
  ClassId(0) fallback — explicitly a PARTIAL fix with an open residual),
  `d8e79b434` (XStream verifier common-superclass merge), `3ea76429a` (Jasper
  JDT interpret ban), plus origin/dev merge contents.

The `[GC-ARRAY-GUARD] array_length(non-array) ... stored_len=40` line matches
the `synthetic_native_wrong_layout_corrupts_adjacent_object` family the
FormAuthenticator investigation left open (cross-ref: its "partial fix landed"
residual, `c3c2b9ee2` doc commit). This repro is deterministic and fast (~1s),
so it is likely the best bisection vehicle that family has had so far.

## Repro

```bash
export MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1
CP=$(cat apps/commons-math/.cm-cp-windows.txt)
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --Xmx 2g -cp "$CP" org.junit.platform.console.ConsoleLauncher execute \
  --select-class org.apache.commons.math4.legacy.core.dfp.DfpTest \
  --details=summary --disable-banner
```

Expected (HotSpot, and 0975a07ad): `78 tests successful`.

## Next step

Bisect the non-merge candidates above with this repro (each has a prebuilt
sibling worktree or is one rebuild away); start with `686de27c1` since its own
commit message declares itself partial and the guard signature matches.
