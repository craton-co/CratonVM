# WildFly — `Collections.emptyList()` not the `EMPTY_LIST` singleton

## Status
**FIXED** on `fix/wildfly-enum-constants` (`native-collections/src/lib.rs`, commit `e1ae165`).

## Severity
**HIGH** — WildFly controller test boot builder throws on every subsystem test.

## App / suite
- **Test:** `HealthSubsystemTestCase.testSubsystem[0]`
- **Framework:** `ModelTestBootOperationsBuilder` (WildFly core test harness)

## Symptom (before fix)

```
java.lang.IllegalArgumentException: Boot operations are already set
```

## HotSpot behavior

`Collections.emptyList()` returns the **`Collections.EMPTY_LIST`** singleton (same reference every call). WildFly initializes `bootOperations = emptyList()` then checks `bootOperations != EMPTY_LIST` before allowing `setXml`.

## CratonVM behavior (before fix)

`emptyList()` / `emptyMap()` / `emptySet()` allocated **new** empty collection instances each call. CratonVM populated static `EMPTY_*` fields via real `<clinit>`, but factories did not return them → **`emptyList() == EMPTY_LIST` was false** → guard always tripped.

## Root cause (confirmed)

Factory natives in `native-collections` returned synthetic `ArrayList`/`HashMap`/`HashSet` instead of reading static `Collections.EMPTY_LIST` / `EMPTY_MAP` / `EMPTY_SET`.

## Fix

Factories now return the static empty singleton fields. Verified EmptyId 7/7 vs HotSpot (was 4/7). Test advances past boot-operations guard.

## Reproduce

```java
import java.util.*;
public class EmptySingletonProbe {
    public static void main(String[] a) throws Exception {
        var f = Collections.class.getDeclaredField("EMPTY_LIST");
        f.setAccessible(true);
        Object empty = f.get(null);
        System.out.println(Collections.emptyList() == empty);
    }
}
```

Expect `true`.

## Related

- [bug-wildfly-jaxp-premature-end-of-file.md](bug-wildfly-jaxp-premature-end-of-file.md) (next failure layer in same test class)
