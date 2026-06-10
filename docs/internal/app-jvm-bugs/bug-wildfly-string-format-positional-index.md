# WildFly — `String.format` ignores positional index `%N$`

## Status
**FIXED** on `fix/wildfly-enum-constants` (`native-builtins/src/lang_string.rs`).

## Severity
**HIGH** — resource paths and subsystem XML filenames wrong; tests fail assertions.

## App / suite
- **App:** WildFly health subsystem tests
- **Test:** `HealthSubsystemTestCase.testSchema` / resource loading
- **Logs:** wildfly four-apps suite after enum fix

## Symptom (before fix)

```
java.lang.AssertionError: subsystem_%2$d_%3$d.xml url is null
```

WildFly builds resource names with:

```java
String.format(Locale.ROOT, "subsystem_%2$d_%3$d.xml", name, 1, 0, stability);
```

Expected: `subsystem_1_0.xml`. CratonVM returned literal `subsystem_%2$d_%3$d.xml` → `Class.getResource` null.

## HotSpot behavior

Positional format specifiers `%2$d`, `%3$d` substitute arguments 2 and 3 (1-based). Resource loads correctly.

## CratonVM behavior (before fix)

`native_string_format` parsed `%2$d` as width=`2`, conversion=`$` (invalid) → emitted spec literally, consumed no arguments.

Also broken: relative flag `%<` (not exercised in this exact assertion but fixed in same patch).

## Root cause (confirmed)

Formatter parser in `lang_string.rs` lacked **`%[argument_index$]`** and **`%<`** handling per `java.util.Formatter` spec.

## Fix

Parse explicit 1-based index before flags/width/precision. Implement relative `%<`. Verified 11/11 format cases including positional and ordinary specifiers.

## Reproduce

```java
import java.util.Locale;
public class FormatPosProbe {
    public static void main(String[] a) {
        System.out.println(String.format(Locale.ROOT, "subsystem_%2$d_%3$d.xml", "n", 1, 0, 1));
    }
}
```

Expect `subsystem_1_0.xml`.

## Related

- [bug-hibernate-log-format-placeholder.md](bug-hibernate-log-format-placeholder.md) (ordinary `%s` may still fail on main build)
- [bug-wildfly-enum-getenumconstants-ecj.md](bug-wildfly-enum-getenumconstants-ecj.md)
