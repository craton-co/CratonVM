# CratonVM vs HotSpot Divergence Log

This document tracks known behavioral divergences between CratonVM and HotSpot
(OpenJDK 25) discovered through differential testing (`vm/tests/differential.rs`).

Each entry records the class, method, expected (HotSpot) behavior, actual
(CratonVM) behavior, root cause, and resolution status.

## How to add entries

Run the differential test suite and check the generated report:

```
cargo test -p cratonvm-vm --test differential -- --ignored --nocapture
```

Divergences are written to `bench/differential-divergences.json`. Copy notable
findings into this log with analysis.

---

## Active Divergences

### DIV-001: String.toUpperCase locale sensitivity

| Field     | Value |
|-----------|-------|
| Class     | `cratonvm/DiffString` |
| Method    | `toUpperCase()` |
| HotSpot   | Uses default locale rules (e.g. Turkish dotted-I) |
| CratonVM   | ASCII-only uppercasing in synthetic String stub |
| Status    | Open |
| Severity  | Low (ASCII inputs match; locale-dependent inputs diverge) |

**Root cause:** The synthetic `String.toUpperCase()` implementation uses a
Rust-side ASCII uppercase conversion. HotSpot delegates to ICU / JDK locale
tables. For pure ASCII test inputs the results match; locale-sensitive inputs
(Turkish, German eszett) will diverge.

**Resolution path:** Implement locale-aware case mapping in the native String
bridge, or load real JDK String classes in non-synthetic mode.

---

### DIV-002: System.out.println formatting of floating-point values

| Field     | Value |
|-----------|-------|
| Class     | Various |
| Method    | `System.out.println(double)` |
| HotSpot   | Uses `Double.toString()` spec (shortest representation) |
| CratonVM   | Uses Rust `format!("{}")` (may differ in trailing zeros) |
| Status    | Open |
| Severity  | Medium (affects any test printing doubles) |

**Root cause:** Rust's default float formatting does not match Java's
`Double.toString()` specification (which uses the "shortest representation
that round-trips" algorithm from Ryu). For example, `3.14` vs `3.14` may
match, but `0.1 + 0.2` representations could differ.

**Resolution path:** Implement Java-spec-compliant `Double.toString()` and
`Float.toString()` in the native bridge using the Ryu algorithm.

---

### DIV-003: Exception message text differences

| Field     | Value |
|-----------|-------|
| Class     | Various |
| Method    | Exception-throwing paths |
| HotSpot   | Detailed NullPointerException messages (JEP 358) |
| CratonVM   | Generic or simplified exception messages |
| Status    | Open |
| Severity  | Low (does not affect control flow, only message text) |

**Root cause:** HotSpot (since JDK 14, JEP 358) generates helpful NPE
messages like "Cannot invoke method X on null reference." CratonVM generates
simpler messages. This does not affect exception type or control flow, only
the `getMessage()` return value.

**Resolution path:** Implement JEP 358 enhanced NPE messages in the
exception subsystem.

---

## Resolved Divergences

_No resolved divergences yet. Entries will be moved here as fixes land._

---

## Testing methodology

The differential harness (`vm/tests/differential.rs`) works as follows:

1. For each test method, invoke it under CratonVM via `Vm::invoke()` and
   capture `printed_lines` + return value.
2. Invoke the same method under HotSpot via `java` subprocess and capture
   stdout + printed return value.
3. Compare outputs. Mismatches are recorded in `DivergenceReport` and
   serialized to `bench/differential-divergences.json`.

Tests are organized into:
- `diff_basic_arithmetic` -- integer arithmetic (T4.10.2)
- `diff_string_operations` -- String methods (T4.10.3)
- Batch runs via `DIFFERENTIAL_CLASSES` env var
