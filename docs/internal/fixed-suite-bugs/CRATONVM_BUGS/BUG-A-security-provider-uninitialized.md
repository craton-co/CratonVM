# Bug A — security `Provider` stubs report `initialized=false` → `IllegalStateException` poisons `SessionIdGeneratorBase`

**Severity:** High (dominant Tomcat-suite breaker — affects nearly every Catalina/Coyote integration test that creates a session manager).
**Status on CratonVM:** crash/poison. **HotSpot:** clean.
**Run date:** 2026-06-11
**Binary under test:** `C:\craton\CratonVM\target\release\cratonvm.exe` (dev `c8f3bb3a`)

## Symptom

Many server tests die right after the JUnit banner with no test output. The
VM log shows:

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError
     class=org/apache/catalina/util/SessionIdGeneratorBase cause=java/lang/IllegalStateException
...
Error in thread "main" linkage error: no class def found: org/apache/catalina/util/SessionIdGeneratorBase
```

Once the `<clinit>` of `SessionIdGeneratorBase` fails, the class is permanently
poisoned: every later use throws `NoClassDefFoundError`. Tomcat creates a
`SessionIdGenerator` in `ManagerBase.startInternal()`, so essentially every test
that starts a web application is affected.

## Minimal reproduction

```java
public class Touch {
  public static void main(String[] a) throws Exception {
    Class.forName("org.apache.catalina.util.SessionIdGeneratorBase"); // ISE on CratonVM, OK on HotSpot
  }
}
```

```
HotSpot : LOADED OK
CratonVM: java.lang.ExceptionInInitializerError
          caused by java.lang.IllegalStateException
            at java.security.Security.getAlgorithms(Security.java:1015)
            at java.security.Provider.keys(Provider.java:664)
            at java.security.Provider.checkInitialized(Provider.java:683)
            at org.apache.catalina.util.SessionIdGeneratorBase.<clinit>(SessionIdGeneratorBase.java:56)
```

`SessionIdGeneratorBase.<clinit>` (line 56) calls
`Security.getAlgorithms("SecureRandom")`, which iterates every provider and
calls `Provider.keys()`. `keys()` runs `checkInitialized()`, which throws a bare
`IllegalStateException` when the provider's `initialized` flag is false.

## Root cause

`Security.getProviders()` on CratonVM returns **bare, uninitialized
`java.security.Provider` instances**. Probe:

```java
for (Provider p : Security.getProviders()) {
    try { p.keys(); System.out.println(p.getName()+" OK size="+p.size()); }
    catch (Throwable t) { System.out.println(p.getName()+" FAIL "+t); }
}
```

| Provider   | HotSpot                                   | CratonVM (before fix)              |
|------------|-------------------------------------------|------------------------------------|
| SUN        | `sun.security.provider.Sun`, size=251     | `java.security.Provider`, **FAIL IllegalStateException** |
| SunRsaSign | `sun.security.rsa.SunRsaSign`, size=84    | `java.security.Provider`, **FAIL** |
| SunEC      | `sun.security.ec.SunEC`, size=158         | `java.security.Provider`, **FAIL** |
| SunJCE     | `com.sun.crypto.provider.SunJCE`, size=494| `java.security.Provider`, **FAIL** |
| …(all 13)… | real populated provider classes           | empty bare `Provider`, **FAIL**    |

CratonVM synthesises the provider list with placeholder `java.security.Provider`
objects (`native-builtins/src/jca/provider_chain.rs::make_provider`). Because the
real `java.security.Provider` constructor never runs for these objects, the
inherited `initialized` boolean stays `false`. Any real `Provider` method guarded
by `checkInitialized()` — `keys()`, `entrySet()`, `elements()`, `getService()`,
and `Security.getAlgorithms(type)` — therefore throws.

The codebase already hit a sibling of this bug for `Provider.getProperty()` (it
registered a native override to dodge `checkInitialized`), but the `keys()` /
`getAlgorithms()` path was left unguarded.

## Fix

`make_provider` now sets the inherited flag after populating the name/version/info
fields:

```rust
// native-builtins/src/jca/provider_chain.rs, make_provider()
ctx.set_field_by_name(p, "initialized", Value::Int(1));
```

The backing Hashtable of these synthetic providers is empty (`count == 0`), so
`keys()` returns an empty enumeration and `getAlgorithms("SecureRandom")` yields
an empty set instead of throwing. Tomcat then takes its documented fallback path
(`DEFAULT_SECURE_RANDOM_ALGORITHM = ""` → platform-default `SecureRandom`) and
logs a single informational warning — graceful degradation rather than a poisoned
class. This is strictly safer than the previous behaviour (an ISE was already a
hard failure for any caller that reached these guards).

## Impact of the fix

`SessionIdGeneratorBase` now initialises, unblocking session-id generation and
therefore the large family of Catalina/Coyote/servlet integration tests that
start a web application. See `RESULTS.md` for the before/after suite delta.
