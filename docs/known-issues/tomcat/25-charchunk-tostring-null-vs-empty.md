# `CharChunk.toString()` returns `""` instead of `null` when empty/recycled

**Status:** OPEN, small and well-isolated. Confirmed CratonVM-only
regression — passes on HotSpot; fully deterministic (fails in well under 1
second, no timing/load dependency, reproduced identically across two runs).

## Symptom

`org.apache.tomcat.util.buf.TestCharChunk.testToString`:

```
java.lang.AssertionError: expected null, but was:<>
	at org.apache.tomcat.util.buf.TestCharChunk.testToString(TestCharChunk.java:77)
```

Test source (`test/org/apache/tomcat/util/buf/TestCharChunk.java:66-77`):

```java
@Test
public void testToString() {
    CharChunk cc = new CharChunk();
    Assert.assertNull(cc.toString());          // <-- fails here or below
    char[] data = new char[8];
    cc.setChars(data, 0, data.length);
    Assert.assertNotNull(cc.toString());
    cc.recycle();
    // toString() should behave consistently for new ByteChunk and
    // immediately after a call to recycle().
    Assert.assertNull(cc.toString());
}
```

A freshly-constructed `CharChunk` (no chars ever set), and a `CharChunk`
immediately after `.recycle()`, must both return `null` from `.toString()`.
CratonVM's implementation returns an empty string (`""`) in at least one of
those two states instead.

## Fix guidance

`org.apache.tomcat.util.buf.CharChunk.toString()` (or whatever native/real
dispatch backs it) needs to check for the "no buffer set" / "recycled"
state explicitly and return `null`, matching upstream Tomcat's real
implementation (`return new String(buff, start, end - start)` guarded by an
`isNull()`-style check on the underlying buffer, not just length-zero).
Small, self-contained fix — worth picking up first among this batch.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 60 -Parallel 1 -RunName charchunk-repro -Exe <cratonvm.exe>
```
