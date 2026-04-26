# T2 Missing-Natives Census (T2.1.4)

This file is the authoritative list of missing-native subtickets for
**Tier 2 — Bootstrap & Real JDK** in `docs/roadmap-100.md`. It is
generated from a combination of:

1. The committed baseline `bench/missing-natives.json` (produced by
   `rustjvm --dump-missing-natives FILE`, schema: flat list).
2. The committed grouped baseline `bench/missing-natives-grouped.json`
   (produced by `rustjvm --dump-missing-natives-grouped FILE`, schema:
   `{ version, modules: { <module-name>: [...] } }`).
3. Manual classification against the roadmap's T2.2 – T2.9 subtickets.

The underlying tooling for items 1 and 2 is implemented in
`vm/src/vm/vm_init.rs` — see `SharedVm::dump_missing_natives_json`,
`SharedVm::dump_missing_natives_grouped_json`, and
`classify_jdk_module`. Unit tests live in the `t2_*` prefix in that
file (8 tests, all green).

## Current baseline (2026-04-15)

Running `rustjvm --classpath bench HelloWorld` against the NEW-11
default feature set (`experimental-tls,experimental-crypto,
experimental-jmx,experimental-serialization,experimental-aot,
experimental-debug` — note: **no** `synthetic-jdk`) produces:

```
{
  "missing_natives": []
}
```

This is **not** zero because the JDK is complete — it is zero because
HelloWorld terminates on a `java/io/PrintStream.println(String)`
*linkage error* (the synthetic `PrintStream` class file does not declare
that method) before any `ACC_NATIVE` method is invoked. The audit log
only records `ACC_NATIVE` methods that have no Rust implementation; it
does not record linkage errors on non-native members.

The census becomes meaningful once the linkage error is fixed (which is
T2.4 territory: `PrintStream.println` overloads) — at that point the
baseline will start collecting real entries as the program progresses
through `java.io`, `java.lang`, and `java.util`.

## How to regenerate

```bash
cargo build -p rustjvm-cli --release
./target/release/rustjvm \
  --classpath bench \
  --dump-missing-natives bench/missing-natives.json \
  --dump-missing-natives-grouped bench/missing-natives-grouped.json \
  HelloWorld
```

For the richer census used to generate the T2.X subticket list below,
swap `HelloWorld` for a Spring Boot JAR or the Renaissance benchmark
suite (both already in `bench/`). Any ACC_NATIVE method that RustJVM
does not implement will appear in both JSON outputs.

## Roadmap-100 T2.X subtickets

The subtickets below map to the atomic steps in `docs/roadmap-100.md`.
Each is "created" in the sense that the roadmap already enumerates
them — this file acts as the *index* from the census output back to
the roadmap item, and records the implementation status observed in
the Session 54 survey (2026-04-15).

| Subticket | Module | Roadmap ref | Status |
|---|---|---|---|
| T2.2 | `java.base` / `java.lang` | `roadmap-100.md` line 360 | **in progress** — see "T2.2 survey" below |
| T2.3 | `java.base` / `java.util` | `roadmap-100.md` line 401 | pending |
| T2.4 | `java.base` / `java.io` + `java.nio` | `roadmap-100.md` line 427 | pending |
| T2.5 | `java.base` / `java.time` | `roadmap-100.md` line 454 | pending |
| T2.6 | `java.base` / `java.security` | `roadmap-100.md` line 475 | pending (crypto subsystem) |
| T2.7 | `java.base` / `javax.net.ssl` | `roadmap-100.md` line 508 | pending (rustls integration) |
| T2.8 | `java.base` / `java.lang.invoke` | `roadmap-100.md` line 538 | pending |
| T2.9 | `java.base` / JNI | `roadmap-100.md` line 557 | pending |
| T2.10 | `synthetic-jdk` flag default flip | `roadmap-100.md` line 581 | ✅ **already done** (NEW-11, 2026-03-28) — the default feature list in `vm/Cargo.toml` already excludes `synthetic-jdk` |
| T2.11 | Real-app smoke tests | `roadmap-100.md` line 596 | gated on T2.2–T2.9 |
| T2.12 | Tier 2 verification | `roadmap-100.md` line 609 | gated on T2.11 |

## T2.10 note — pre-existing work

Inspection of `vm/Cargo.toml:21` shows the default feature list is:

```toml
default = ["experimental-tls", "experimental-crypto", "experimental-jmx",
           "experimental-serialization", "experimental-aot",
           "experimental-debug"]
```

`synthetic-jdk` is **not** present. Comment at line 14 — "`synthetic-jdk`
is intentionally NOT in the default feature set" — confirms this is the
committed NEW-11 state. Therefore T2.10.1–T2.10.5 are satisfied today;
only T2.10.6 (marking NEW-4 ✅ DELIVERED in `roadmap.md`) is outstanding,
and that is a documentation edit not a code change.
