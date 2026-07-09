# TestVirtualContext — virtual classloader resource returns 404 instead of 200

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.loader.TestVirtualContext.testVirtualClassLoader` fails:
```
java.lang.AssertionError: expected:<200> but was:<404>
```
This test exercises Tomcat's "virtual" webapp loader/context mechanism
(serving classes/resources from a location outside the normal webapp
docBase, via a virtual classloader mapping). A request that should resolve
to `200 OK` instead comes back `404 Not Found`, meaning the virtual
classloader either isn't finding the resource CratonVM's webapp resource
resolution expects, or the virtual-path mapping itself isn't being applied
correctly.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName virtctx `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.loader.TestVirtualContext
```

## Recommendation

Check `org.apache.catalina.loader.VirtualWebappLoader`/`VirtualDirContext`
(or whatever mechanism `TestVirtualContext` sets up — read the test source
for exact setup) against CratonVM's `WebResourceRoot`/classloader resource
resolution. Likely candidates: a path-normalization difference (Windows
path separators leaking into a resource lookup key), or CratonVM's
classloader-real-mode resource resolution not consulting the virtual
mapping the same way HotSpot's does. Compare against
`reference_par_classpath_extension_uri_decode`-style prior findings in this
codebase (URI/path decode edge cases have bitten webapp resource resolution
before).
