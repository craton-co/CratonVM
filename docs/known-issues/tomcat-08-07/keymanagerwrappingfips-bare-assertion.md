# TestKeyManagerWrappingFips — bare assertion failure

**Status:** OPEN. **Severity:** low (insufficient detail to assess further
without added logging). **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.tomcat.util.net.TestKeyManagerWrappingFips.testBug64614_01`
fails:
```
1) testBug64614_01(org.apache.tomcat.util.net.TestKeyManagerWrappingFips)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
	at org.junit.Assert.assertFalse(Assert.java:65)
```
Bug 64614 (upstream Tomcat) concerns `KeyManager` wrapping under FIPS-mode
TLS configuration — this test's name (`testBug64614_01`) and its use of
`assertFalse` suggest it checks that some FIPS-related wrapping condition
does *not* hold in a particular configuration; the bare assertion gives no
message to identify which condition unexpectedly evaluated true on
CratonVM.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.tomcat.util.net.TestKeyManagerWrappingFips
```

## Recommendation

Read `TestKeyManagerWrappingFips.testBug64614_01`'s source directly to see
what the `assertFalse` call is checking, then add targeted logging (the
bare assertion currently carries no signal to work from). Given the class
name, this may relate to `KeyManager`/`X509ExtendedKeyManager` wrapping
behavior under CratonVM's TLS/JSSE stack — worth checking against other
TLS `KeyManager`/`chooseClientAlias`-family findings already tracked in
this codebase before assuming an unrelated new bug.
