# `String.substring` out-of-range throws `ArrayIndexOutOfBoundsException`, not `StringIndexOutOfBoundsException`

**Status:** OPEN. Found 2026-08-04 while removing the forced-native
`java/lang/String` policy
([`FIXED` record](../internal/forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md)).
Pre-existing; **unreachable** until then, because a registered native shadowed
`String.substring` at every dispatch site.

**Reproducer:** `probes/StringUtf16HashProbe`, last three lines.

```
                       HotSpot 25                              CratonVM
substring(-1)   StringIndexOutOfBoundsException                ArrayIndexOutOfBoundsException
                msg="Range [-1, 12) out of bounds for length 12"   msg=null
substring(3,2)  StringIndexOutOfBoundsException                ArrayIndexOutOfBoundsException
                msg="Range [3, 2) out of bounds for length 12"     msg=null
charAt(-1)      StringIndexOutOfBoundsException                StringIndexOutOfBoundsException
                msg="Index -1 out of bounds for length 12"          msg=null
```

`charAt` is the control: its bytecode reaches the right exception class, so
this is not "CratonVM cannot throw `StringIndexOutOfBoundsException`". It is
`substring`'s bounds check specifically.

## Why it matters more than a message would

`StringIndexOutOfBoundsException` and `ArrayIndexOutOfBoundsException` are
siblings under `IndexOutOfBoundsException`, so `catch (IndexOutOfBoundsException)`
behaves identically and `catch (StringIndexOutOfBoundsException)` — which real
parsing and validation code does write — **does not catch this**. A wrong
exception class changes control flow; a missing message only degrades a log
line.

## Where to look

JDK 21+ `String.substring` delegates its bounds check to
`Preconditions.checkFromToIndex(begin, end, length, Preconditions.SIOOBE_FORMATTER)`,
where `SIOOBE_FORMATTER` is a `BiFunction` built by a lambda in
`jdk.internal.util.Preconditions`' static initialiser. Two candidate causes,
and they are distinguishable:

1. **The formatter is null** — its `<clinit>` did not run, or the lambda did
   not materialise — so the precondition machinery falls back to a generic
   exception rather than the string-specific one;
2. **the bounds check is skipped entirely** and the fault comes from the raw
   `Arrays.copyOfRange` underneath, which throws
   `ArrayIndexOutOfBoundsException` by construction.

The null message points at (2) or at a formatter that produced nothing. Check
whether `Preconditions.checkFromToIndex` is reached at all before assuming
which.

Worth checking in the same pass: CratonVM does **not** run
`java/lang/String.<clinit>` — `vm/src/vm/vm_object.rs::pre_init_string_statics`
sets `COMPACT_STRINGS`, `LATIN1` and `UTF16` by hand and skips the rest,
deliberately, because running it "would cascade into loading many classes". Any
`String` static beyond those three is therefore null, and the real bytecode is
entitled to assume they are not.

## Verification when fixed

`probes/StringPolicyMatrixProbe` rows 83-88 (`substring1(PLAIN,-1)`,
`substring1(PLAIN,len+1)`, `substring2(PLAIN,3,2)`, `substring2(PLAIN,-1,3)`,
`substring2(PLAIN,0,len+1)`, `substring2(PLAIN,MIN,MAX)`) plus `new
String(utf8,-1,2,"UTF-8")` and `new String(utf8,0,999,"UTF-8")`. All eight want
the class **and** the message; getting the class right is the part that changes
behaviour.
