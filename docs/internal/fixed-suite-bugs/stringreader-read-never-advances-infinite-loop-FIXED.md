# `StringReader.read()` never advances position → infinite loop — FIXED

Status: FIXED (branch `fix/stringreader-read-never-advances-20260709`, commit
`9b8fd95f`, merged into `dev`)

Date observed: 2026-07-09
Date fixed: 2026-07-09

## Original symptom

```java
StringReader sr = new StringReader("hello");
int n;
while ((n = sr.read()) != -1) { System.out.println((char) n); }
```
Never terminated — every call to `read()` returned `'h'` (104) again. Same
result through a bare `StringReader` or a `BufferedReader` wrapping one.
Tomcat's `RewriteValve.parse(BufferedReader)` (reads the `.rewrite` rules
config character-by-character) hit this on `TestRewriteValve`'s in-memory
rule-file fixture and spun forever.

## The "phantom native dispatch" mystery, resolved

The original investigation (see git history of this doc) found two
candidate registrations for `java/io/StringReader.read()I`
(`native-builtins/src/phases_early.rs`'s `register_scanner_natives`, and
`native-io/src/lib.rs`'s `register_string_rw_natives`) and, via an
unconditional `eprintln!` placed in each closure, concluded **neither** ran
— both appeared to be dead code gated behind the `synthetic-jdk` Cargo
feature, which the default `cratonvm-cli` build doesn't enable. That
conclusion was **half right, half wrong**:

- `native-builtins/src/phases_early.rs`'s copy really is dead — it is only
  reachable via `register_synthetic_overrides`, which genuinely is
  `#[cfg(feature = "synthetic-jdk")]`-gated in the `native-builtins` crate,
  confirmed by re-reading the `#[cfg]` attribute directly above the function
  definition. The `eprintln!` there correctly never fired.
- `native-io/src/lib.rs`'s copy is **not** dead. `native-io`'s
  `register_io_natives` is called **unconditionally** from
  `vm/src/vm/vm_init.rs`, inside the `#[cfg(not(feature = "synthetic-jdk"))]`
  arm (the real-JDK/default-CLI path) as well as the synthetic-jdk arm.
  Within it, `register_string_rw_natives`'s `java/io/StringReader` block is
  itself unconditional (only the neighboring `StringWriter` block and the
  base-`Reader` overloads are `#[cfg(feature = "synthetic-jdk")]`-gated).
  **The original investigation's `eprintln!` for this one must have been
  placed in a closure that genuinely wasn't the live one, or was lost during
  a rebuild** — direct empirical tracing (see below) shows it unambiguously
  firing on every call.

Found the live path by re-instrumenting `NativeMethodRegistry::find` (one
print site covers all ~15 call sites in the interpreter) plus
`ClassManager::load_class`/`create_synthetic_stub`, rebuilt, and reran the
minimal repro:
```
[SRDBG-LOAD] find_class_bytes_delegated(java/io/StringReader) -> Ok(len=2059)
```
Real bytecode loads successfully — `create_synthetic_stub` is never called.
Yet:
```
[SRDBG-FIND] java/io/StringReader.read()I -> HIT (fast path)
```
`native_methods.find` returns a hit every time, on a **real, bytecode-loaded**
`StringReader`. The reason: `native_sr_read`/`native_sr_init` are registered
under `NativeKind::SyntheticStub`, and CratonVM's dispatcher prefers a
`SyntheticStub` native over real bytecode **by default** — the
`CRATONVM_REAL` differential switch (`RealSelector` in
`vm/src/runtime/env_cache.rs`) must be explicitly set (`CRATONVM_REAL=all` or
`CRATONVM_REAL=java/io/StringReader`) to make real bytecode win. Unset (the
default), `RealSelector::prefers_real` returns `false` for every class, so
the native always wins — exactly the opposite of what a nearby code comment
("real JDK bytecode still wins when loaded from a real java.base") claimed.

## Root cause

`native_sr_init`/`native_sr_read`/etc. (native-io/src/lib.rs) stored
content/position/length in flat object field slots `0`/`1`/`2`, matching
only the *synthetic* stub class's 3 generic `_f0.._f2` `Ljava/lang/Object;`
slots (`classloading/src/class_manager.rs::instance_fields(3)`).

Real JDK 25's `java.io.StringReader` was rewritten (confirmed via
`javap -p java.io.StringReader`) to hold a single
`private final java.io.Reader r` delegate — none of the classic
`str`/`length`/`next`/`mark` fields survive. Direct field-level tracing
(`ctx.get_field`/`set_field` logged around every call) showed:

```
[SRDBG-INIT] after-set: content=Object(Some(...)) pos=Object(None) length=Object(None)
```

Field `0` (content) round-tripped fine (an `Object` write into whatever the
real class's field 0 happens to be). Fields `1`/`2` (pos/length) did **not**
— writing `Value::Int` into them read back as `Value::Object(None)`, the
zero-value for a reference-typed slot, because the real class's field
layout at those indices is nothing like the synthetic 3-slot scheme. So
`read()` always saw `pos == 0` and returned the same first character
forever — never the "advance by code-point value instead of UTF-8 width"
bug originally hypothesized (that logic bug was real but lived only in the
already-dead `phases_early.rs` copy, and even there would have manifested
as "truncates to empty then returns -1 on the second call," not "never
advances" — a mismatch the original doc flagged but couldn't resolve for
lack of the live implementation).

## Fix

`native-io/src/lib.rs`: replaced the flat-field-index scheme with a
GC-stable side table (`SR_STATE`, keyed by `identity_hash_code`), the same
pattern already used a few hundred lines up in the same file for
`InputStreamReader`'s `ISR_PENDING` carry-over state. `native_sr_init`,
`native_sr_read`, `native_sr_read_chars`, `native_sr_ready`,
`native_sr_skip`, and `native_sr_reset` now all read/write
`SrState { units: Vec<u16>, pos: usize }` instead of object fields, so they
work identically whether the receiver is the synthetic stub or a real
bytecode-loaded object of whatever internal shape a given JDK uses.

Also removed the always-dead duplicate `java/io/StringReader` /
`java/io/StringWriter` registrations in
`native-builtins/src/phases_early.rs::register_scanner_natives` — proven
unreachable in every build configuration (even a synthetic-jdk build calls
`register_io_natives`, which registers the same triples, *after*
`register_synthetic_overrides`, and `NativeMethodRegistry::register` is a
plain last-write-wins `HashMap::insert`). Leaving known-dead, buggy code
there is exactly what sent the original investigation down a blind alley;
removing it prevents a repeat.

## Verification

- Minimal repro (`StringReaderProbe.java`, `StringReaderProbe2.java`):
  reads `h`,`e`,`l`,`l`,`o` then terminates with `n=-1` (previously: infinite
  `h`,`h`,`h`,...).
- Extended probe covering `read(char[],int,int)`, `ready()`, `skip()`,
  `reset()`, and two independent readers for cross-contamination: all match
  expected `java.io.StringReader` semantics.
- `org.apache.catalina.valves.rewrite.TestRewriteValve` (the suite that
  originally surfaced this): previously hung completely, zero test progress,
  even past a 300s timeout. After the fix, completes all 121 tests well
  within a 280s timeout. Remaining failures in that run (`this.lock is null`
  NPEs in the NIO socket/lock path) are a separate, pre-existing issue
  unrelated to `StringReader` — not investigated further here.

## Related

The sibling "phantom native dispatch" bug (`ByteBuffer.allocate()` never
setting `Buffer.address`) was independently fixed by a concurrent session —
see `docs/internal/tomcat-08-07/bytebuffer-address-unset-aioobe.md` and the
`docs/known-issues/README.md` entry for it. The general lesson from this
investigation applies to that one too: before concluding a registration is
dead from an `eprintln!` that never fired, re-check *every* call chain
(`register_essential_natives` vs `register_synthetic_overrides` vs bare
crate-level `register_io_natives`) rather than trusting an initial
line-number reference, since this codebase is large enough that a doc's
cited `~line N` can drift out of date across concurrent commits — and
double-check whether a `SyntheticStub`-tagged native is unconditionally
preferred over real bytecode by default (it is, via `CRATONVM_REAL`) before
assuming "real bytecode wins" from a code comment alone.
