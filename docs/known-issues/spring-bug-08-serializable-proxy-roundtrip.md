# spring-bug-08: serializable JDK-proxy round-trip broken (`SerializableTypeWrapper`)

| | |
|---|---|
| **Category** | VM-CORRECTNESS (proxy + serialization) |
| **Module** | spring-core |
| **Test class** | `org.springframework.core.SerializableTypeWrapperTests` |
| **CratonVM** | FIXED — serializable JDK-proxy round-trip now works |
| **HotSpot JDK 25** | OK (8/8) |
| **CratonVM HEAD** | branch `fix/spring-bug-08-proxy-serial` (off dev 77f17d99) |
| **Status** | **FIXED** |
| **Suggested owner** | done |

## FIX (landed on `fix/spring-bug-08-proxy-serial`)

Root cause was three independent gaps in how CratonVM's *synthetic* proxy model
(`Proxy$Instance` super + generated `$ProxyN` subclasses) interacts with the real
`ObjectOutputStream`/`ObjectInputStream` bytecode:

1. **Handler never serialized (write side).** The synthetic super
   `java/lang/reflect/Proxy$Instance` neither implemented `java.io.Serializable`
   nor declared the `h` `InvocationHandler` field. The *real* JDK
   `java.lang.reflect.Proxy` does both. Because of that, real-OOS
   `ObjectStreamClass.lookup(Proxy$Instance, /*all=*/false)` returned `null`
   (non-Serializable → no descriptor), the proxy's `superDesc` was written as
   `TC_NULL`, and the handler field was dropped entirely (52-byte stream vs
   HotSpot's ~200). **Fix:** `ensure_synthetic_class` now makes `Proxy$Instance`
   implement `Serializable` and declare `h` at slot 0 (the slot the proxy natives
   already use for the handler) — so the real OOS serializes the handler via the
   `superDesc` chain, exactly like HotSpot's `Proxy.h`.
   (`classloading/src/class_manager.rs`)

2. **`isProxyClass(Proxy$Instance) == true` (write side, "Circular reference").**
   The super `Proxy$Instance` is the analog of `java.lang.reflect.Proxy`, which is
   NOT itself a proxy class (`Proxy.isProxyClass(Proxy.class) == false`). Returning
   `true` for it made OOS write the proxy's `superDesc` as a *second*
   `TC_PROXYCLASSDESC` (with the same interfaces) instead of the non-proxy
   `Proxy$Instance` descriptor that carries `h`; on read both descriptors resolved
   to the same `$ProxyN` name, tripping `ObjectStreamClass`'s
   "Circular reference." guard. **Fix:** the `Proxy.isProxyClass` native now
   excludes the exact `Proxy$Instance` class (only generated `$ProxyN` *subclasses*
   are proxy classes). (`native-builtins/src/lib.rs`)

3. **`Module.defineModule0` crash (read side).** `ObjectInputStream`'s default
   `resolveProxyClass` routes `Proxy.getProxyClass` →
   `ProxyBuilder.getDynamicModule` → `Module.defineModule0` — a native the
   synthetic proxy model can't satisfy (`UnsatisfiedLinkError`, surfaced as the
   `ClassNotFoundException: null` in the original report). **Fix:** force-override
   `ObjectInputStream.resolveProxyClass(String[])` (registered as a `Bridge`
   native so it survives the no-synthetic-stubs drop, and listed in
   `force_native_over_real_jdk_bytecode`) to return a CratonVM generated `$ProxyN`
   class for the stream's interface set — keeping the whole round-trip on
   CratonVM's own proxy machinery. (`native-builtins/src/lib.rs`,
   `vm/src/runtime/interpreter.rs`; mirrored in the gated
   `native-builtins/src/serialization.rs` for `experimental-serialization` builds)

Read-side instance creation + `h` restore reuse the existing
`ReflectionFactory.newConstructorForSerialization` path (the same one keycloak's
`SkeletonKeyTokenTest.testSerialization` exercises).

### Verified
- `RawProxy` (pure-JDK serializable proxy round-trip) — CratonVM == HotSpot
  (handler restored, `equals` both ways true).
- `SpringProxy2` (real `org.springframework.core.SerializableTypeWrapper.forField`
  round-trip, i.e. exactly what `SerializableTypeWrapperTests` does) — **2/2 ==
  HotSpot** (`List<String>`, `Map<String,Integer>`).
- Proxy regression probes (dispatch, `isProxyClass` true/false, collections via
  handler-routed `hashCode`/`equals`, `getInvocationHandler`, `ProxyProbe`) — all
  match HotSpot. (Spring framework test sources are not vendored in this checkout,
  so the round-trip is reproduced via the real `spring-core` jar.)

## Symptom
Surfaced only **after** [[spring-bug-05]] (dynamic proxy creation) was fixed. Spring's
`SerializableTypeWrapper` wraps `java.lang.reflect.Type` in a serializable JDK dynamic proxy
(`SerializableTypeProxy`) so a `Type` can survive `ObjectOutputStream`→`ObjectInputStream`.
Under CratonVM the round-trip fails:
```
typeVariableType()  -> java.lang.ClassNotFoundException: null
wildcardType(), forField(), forConstructor(), forMethodParameter() -> AssertionFailedError
```
`ClassNotFoundException: null` ⇒ deserialization reads a null/blank class name (the proxy's
`Class[]` interfaces or the proxy class descriptor isn't serialized/resolved correctly). The
`AssertionFailedError`s ⇒ the deserialized `Type` doesn't equal the original.

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-core/build/cratonvm-testcp.txt)"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.core.SerializableTypeWrapperTests
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.core.SerializableTypeWrapperTests   # 8/8 OK
```

## Suspected root cause
CratonVM's dynamic-proxy serialization support. Either (a) `ObjectStreamClass`/`writeObject` for a
`$ProxyN` class doesn't emit the proxy's interface list the way `ObjectInputStream.resolveProxyClass`
expects, or (b) `Proxy.getProxyClass`/`newProxyInstance` during deserialization receives a null
interface name. Inspect CratonVM's `java.io.ObjectInputStream` proxy-class resolution
(`resolveProxyClass`) and how proxy classes are described by `ObjectStreamClass`.

## Notes
Distinct from [[spring-bug-05]] (which only gated proxy *creation*). Lower priority than the
annotation/stream bugs; good handoff. Verify after bug-05 is merged to dev.
