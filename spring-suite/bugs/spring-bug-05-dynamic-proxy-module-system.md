# spring-bug-05: `InternalError: Proxy is not supported until module system is fully initialized`

| | |
|---|---|
| **Category** | VM-CORRECTNESS (dynamic proxy / bootstrap) |
| **Module** | spring-core |
| **CratonVM** | FAIL — `java.lang.InternalError` creating a JDK dynamic proxy |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (dev) |
| **Status** | **FIXED on dev** — commit `16f832c0` (merge `d75f7716`) |
| **Suggested owner** | me (done) |

## FIX LANDED + verified
Fix in `vm-cli/src/main.rs` (before `main()`, real-JDK mode): invoke the un-shadowed real
`jdk/internal/misc/VM.initLevel(I)V` setter to advance the static field to SYSTEM_BOOTED (4).
Verified with a minimal probe — dynamic proxy now works:
```
$ cratonvm(worktree) ProxyProbe   ->   invoked run / PROXY_OK   (was: InternalError)
```
**Note — `SerializableTypeWrapperTests` was mis-attributed here.** bug-05 fixed its Proxy
`InternalError` (it went from all-fail to 1/8 pass), but its remaining 7 failures are a *separate*
proxy-**serialization** bug (`ClassNotFoundException: null` + type round-trip mismatch) tracked as
**[[spring-bug-08]]**, not bug-05.

## Symptom
```
java.lang.InternalError: Proxy is not supported until module system is fully initialized
  (at java.lang.reflect.Proxy.newProxyInstance time)
```
`Proxy.newProxyInstance` (JDK dynamic proxy) refuses to run because CratonVM reports the module
system as not fully initialized. HotSpot has the module system up by then.

## Affected test classes (confirmed CV-unique, HotSpot OK)
```
core.SerializableTypeWrapperTests
```
(JDK dynamic proxies are pervasive in Spring — AOP, `@Configuration` CGLIB fallbacks, mapper
interfaces. Expect this to gate many spring-context / spring-aop / spring-tx tests later.)

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-core/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.core.SerializableTypeWrapperTests
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.core.SerializableTypeWrapperTests   # passes
```

## Root cause — CONFIRMED
From JDK 25 `lib/src.zip`:
- `java/lang/reflect/Proxy$ProxyBuilder` (Proxy.java:548):
  ```java
  if (!VM.isModuleSystemInited())
      throw new InternalError("Proxy is not supported until module system is fully initialized");
  ```
- `jdk/internal/misc/VM` (VM.java):
  ```java
  private static final int MODULE_SYSTEM_INITED = 2;
  private static volatile int initLevel;                 // line 51 — raw static field
  public static void initLevel(int value) { … }          // setter, called during real initPhase1/2/3
  public static boolean isModuleSystemInited() { return initLevel >= MODULE_SYSTEM_INITED; }  // reads FIELD
  ```
`isModuleSystemInited()` reads the **static field `initLevel` directly** — it does NOT call the
`initLevel()` method. CratonVM registers a native for the `VM.initLevel()` *method* (returns
`max(2)`), but that is **irrelevant** here. The field is only advanced by the `VM.initLevel(int)`
*setter*, which the real `System.initPhase1/2/3` invoke. CratonVM **shadows `initPhase2`/`initPhase3`
with no-op natives** (`native-builtins/src/lang_system.rs:1799` & `:1811`) that return success
without ever advancing the field — so `jdk.internal.misc.VM.initLevel` stays `< 2` and every JDK
dynamic proxy is rejected.

## Fix (planned — apply + build after baseline run)
When booting real JDK bytecode, advance the **real** `jdk/internal/misc/VM.initLevel` static field.
Cleanest: wherever CratonVM calls its internal `set_init_level` during boot
(`vm/src/vm/vm_init.rs` ~4285, levels 1→4), also write the static `initLevel` field of the loaded
`jdk/internal/misc/VM` class to the same value (and notify `awaitInitLevel` waiters). Equivalent
minimal fix: in `native_system_init_phase2` set the field to 2, in `native_system_init_phase3` set
it to 4 — but syncing at `set_init_level` is more robust and also fixes any other code gated on the
real field. Verify with the 1-line trigger:
`java.lang.reflect.Proxy.newProxyInstance(cl, new Class[]{Runnable.class}, (p,m,a)->null)`.

## Notes
Small, targeted fix; large blast radius (dynamic proxies underpin Spring AOP, `@Configuration`,
mapper interfaces, JDK-proxy-based beans). High value — fix early.
