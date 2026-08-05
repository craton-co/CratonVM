# Flyway CGLIB heap corruption and SIGSEGV — RESOLVED

**Status: FIXED (2026-07-12).**

## Root cause

`NativeContextImpl::read_string` identified a String from the heap header's
class ID alone. CratonVM reference arrays store their **component** class ID in
that header, so a `String[]` was accepted as a `java/lang/String`.

The annotation-proxy path (`annotation_element_to_java_typed` ->
`create_annotation_proxy` -> `native_class_get_declared_annotations`) creates a
one-element `String[]`. The structural String reader then read its array element
as a 16-byte object field: the reference became the payload pointer and the next
header word became `Value` tag 6. This produced the apparent malformed String
with `num_slots=0` and the later `HashSet.add` / `Map.hashCode` corruption
signature. The malformed value predated those collection calls; they only read
it.

`read_string` now requires `ObjectKind::Object` before checking the String class
identity or decoding String fields. Arrays, including `String[]`, are rejected.

## JIT residual — the ban is GONE, and HSQLDB is JIT-eligible again

After the heap fix, default JIT still had an independent HSQLDB SIGSEGV during
the Flyway integration. `CRATONVM_JIT_DENY=org/hsqldb/` consistently completed
the class, so HSQLDB was kept interpreted by default at both VM eligibility
and the JIT crate's final admission gate.

**That ban no longer exists** (updated 2026-08-05). `d1979bec5` (2026-08-01,
"delete the static ban machinery outright") deleted `vm/src/jit/skip_list.rs`
and all four of its mirrors at `try_compile`'s final admission gate, including
both `org/hsqldb/` entries. `CRATONVM_JIT_ALLOW_PACKAGES` was deleted with
them — it existed only to lift these bans. The single force-interpret lever is
now `CRATONVM_JIT_DENY` (substring match on `Class.method`).

Leaving this paragraph in the present tense cost a day: the 2026-08-05
`FlywayAutoConfigurationTests` timeout page cited it to conclude that "this
run's HSQLDB path is running interpreted", when HSQLDB had been JIT-eligible
for four days, and looked for the stall inside HSQLDB. It was not there — see
`docs/internal/fixed-suite-bugs/springboot/flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`.

The SIGSEGV this ban existed for does not reproduce with HSQLDB JIT-eligible:
`FlywayAutoConfigurationTests` — 73 Spring contexts, each running Flyway
against in-memory HSQLDB — passes 73/73 under default JIT on both Windows
(214s/221s) and Azure Linux (224s/232s), with no SIGSEGV, heap-cell
corruption, or malformed-String diagnostic.

## Validation

- Release build: `cargo build --release -p cratonvm-cli --bin cratonvm`.
- Default-JIT isolated Spring Boot runner probe:
  `flyway-cglib-defaultfix-20260712/defaultfix`.
- Result: completed `FlywayAutoConfigurationTests` in 139.7s with normal test
  failures and **no SIGSEGV**, heap-cell corruption, out-of-bounds String read,
  or malformed `num_slots=0` String diagnostic.
- The previous default-JIT probe crashed in about 31–33s; an interpreted
  threshold control also completed normally.
