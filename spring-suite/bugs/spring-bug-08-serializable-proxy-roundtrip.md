# spring-bug-08: serializable JDK-proxy round-trip broken (`SerializableTypeWrapper`)

| | |
|---|---|
| **Category** | VM-CORRECTNESS (proxy + serialization) |
| **Module** | spring-core |
| **Test class** | `org.springframework.core.SerializableTypeWrapperTests` |
| **CratonVM** | FIXED — serializable JDK-proxy round-trip now works |
| **HotSpot JDK 25** | OK (8/8) |
| **CratonVM HEAD** | branch `fix/spring-bug-08-proxy-serial` (off dev 77f17d99) |
| **Status** | **FIXED** (full write-up: `docs/known-issues/spring-bug-08-serializable-proxy-roundtrip.md`) |
| **Suggested owner** | done |

> FIX SUMMARY: synthetic `Proxy$Instance` super now implements `Serializable` +
> declares the `h` handler field (so OOS serializes the handler via `superDesc`);
> `Proxy.isProxyClass(Proxy$Instance)` returns false (only `$ProxyN` subclasses
> are proxy classes — fixes the "Circular reference" on the duplicated proxy
> desc); and `ObjectInputStream.resolveProxyClass` is force-overridden to return a
> CratonVM `$ProxyN` instead of crashing on `Module.defineModule0`. See the
> known-issues doc for the full root-cause analysis and verification.

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
