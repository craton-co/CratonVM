# Spring Expression double-literal suffix parse failures - FIXED

## Symptom

Spring Expression tests failed with `EL1040E` parse errors for Java floating-point text that uses a type suffix, including examples such as `1d`, `1.25f`, and `6.0221415E+23d`.

## Root Cause

CratonVM routed `Float.parseFloat(String)`, `Float.valueOf(String)`, `Double.parseDouble(String)`, and `Double.valueOf(String)` through Rust `str::parse::<f32/f64>()`. Rust's parser rejects Java's optional single float type suffix (`d/D/f/F`) even though HotSpot accepts it for numeric float/double literals.

## Fix

`../../../../native-builtins/src/lang_math.rs` now strips one trailing Java float type suffix when it follows numeric-looking text, then reuses the existing Rust parser and error path. The guard keeps non-numeric spellings such as `NaNd`, `Infinityd`, repeated suffixes, and underscores rejected, matching HotSpot for the covered suffix cases.

## Verification

- `/home/victor/.cargo/bin/cargo test -p cratonvm-native-builtins parse_float`
- `/home/victor/.cargo/bin/cargo test -p cratonvm-native-builtins parse_double`
- `/home/victor/.cargo/bin/cargo build --release -p cratonvm-cli`
- Minimal CratonVM parse probe with `--java-home /data/data/jdk25-real`: suffix examples now parse for both `Double.parseDouble` and `Float.parseFloat`.
- Spring `KRun` real-JDK/JIT probe: `org.springframework.expression.spel.LiteralTests`, `ArrayConstructorTests`, and `OperatorTests` pass. `ConstructorInvocationTests` still has one unrelated constructor argument-conversion assertion after `new String(3.0d)` parses.

## Residual

The existing parser still does not implement Java hexadecimal floating-point text such as `0x1.0p0d`; this was not the Spring `EL1040E` suffix failure fixed here.
