# Modified classpath override artifact identity is not honored

**Status: OPEN - confirmed 2026-07-18**

`ModifiedClassPathExtensionOverridesTests` and its parameterized counterpart
add Spring Framework 4.1.0 artifacts ahead of the normal Spring Boot 4.1.0
test classpath. They require the nested `ModifiedClassPathClassLoader` to load
`ApplicationContext` and `StringUtils` from those override JARs.

On CratonVM both JIT and `--nojit` instead report the original Spring 7.0.7
JAR locations. The modified loader's URL list is correct and starts with the
4.1.0 artifacts, so this is not artifact resolution or an Aether/network
failure. It is a separate class-identity/redefinition problem after the JUnit
relaunch through the modified loader.

This is intentionally distinct from the resolved
`httpclient-autoconfigure-classpath-presence-cluster`: that bug was a false
positive for excluded dependencies, and all exclusion/fork tests now pass.

## Reproduction

Spring Boot 4.1.0-SNAPSHOT with JDK 25.0.3 and the CratonVM release binary:

`C:\craton\CratonVM-target-httpclient-classpath-20260718-019f733b\release\cratonvm.exe`

Both modes fail identically:

- `test-support/spring-boot-test-support`
  `ModifiedClassPathExtensionOverridesTests` - 2/2 failed
- `test-support/spring-boot-test-support`
  `ModifiedClassPathExtensionOverridesParameterizedTests` - 2/2 failed

The dedicated runner logs are under
`apps/spring-boot-suite-runner/.suite-httpclient-classpath-019f733b/results/httpclient-classpath-019f733b-r6`.
