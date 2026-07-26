# Spring array Class type-name parity - FIXED

## Symptom

`org.springframework.aot.hint.ReflectionTypeReferenceTests` failed for array
class inputs such as `Integer[].class` and `Object[].class`. Spring expected
the HotSpot `Class.getTypeName()` / canonical array rendering
(`java.lang.Integer[]`, `java.lang.Object[]`), while CratonVM exposed the raw
JVM descriptor-style name for those paths.

## Root Cause

CratonVM's class-name helpers treated array descriptors mostly like ordinary
internal class names: slash-to-dot conversion was available for `getName()`,
but `getCanonicalName()`, `getSimpleName()`, and `getPackageName()` did not
fully implement HotSpot's array-specific presentation rules. The result was
descriptor leakage into Spring's type-reference display and binary-name logic.

## Fix

`../../../../native-builtins/src/lang_class.rs` now centralizes array descriptor rendering:
component names are decoded once, `[]` suffixes are appended per dimension,
canonical names convert `$` to `.`, simple names use the component simple
name, and `getPackageName()` follows the component package (`java.lang` for
primitive arrays). `Class.getPackage()` still returns null for array classes,
matching HotSpot.

## Verification

- `/home/victor/.cargo/bin/cargo test -p cratonvm-native-builtins array_descriptor_name_helpers_match_hotspot_surfaces -- --nocapture`
- HotSpot probe over `Integer[]`, `Object[]`, `int[]`, and `String[][]` to pin
  `getName`, `getTypeName`, `getCanonicalName`, `getSimpleName`,
  `getPackageName`, and `getPackage` expectations.
