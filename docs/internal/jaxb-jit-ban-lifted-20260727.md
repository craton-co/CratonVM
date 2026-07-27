# JAXB (`org/glassfish/jaxb/`, `jaxb_mapping_residual_skip_prefix`) — BAN LIFTED

**Status: RESOLVED 2026-07-27. Ban removed from both admission gates.**
Supersedes the 2026-07-26 "CONFIRMED still needed" finding recorded in the
earlier revision of this file (git history), which was correct at the time:
the corruption it re-verified was real, and has since been root-caused and
fixed.

## What the ban guarded

Hibernate mapping metadata initializes JAXB's QName-heavy runtime graph.
JITting `org.glassfish.jaxb` corrupted that graph and produced a self-cast
`QName cannot be cast to QName`. Two guards enforced it:

- `vm/src/jit/skip_list.rs` — `jaxb_mapping_residual_skip_prefix` (VM-side
  eligibility, liftable with `CRATONVM_JIT_ALLOW_PACKAGES=org/glassfish/jaxb/`)
- `jit/src/lib.rs` — `jaxb_mapping_jit_deny_prefix` (final `try_compile`
  admission gate, so background/tiered compiles fail closed too)

Both are gone as of this change, along with their unit tests; a new
`jaxb_package_is_jit_eligible_after_removal` test asserts the package is
admitted under the Conservative policy.

## Root cause

**`82b78bca5` — "fix(jit): String field intrinsics read compact primitive
fields 4 bytes high"** (2026-07-26 13:14 UTC).

The corruption was never JAXB-specific. JAXB's marshaller writes element and
attribute *names* — interned `String`s — through the JIT's `java/lang/String`
field intrinsics. On a compact-layout `String` those intrinsics read the
primitive `coder`/`hash` field four bytes above its real offset, so the
name strings decoded as empty or garbage. Downstream that shows up two ways:

- names vanish from the produced XML (`</widget>` → `</>`, `<widget id=…>` →
  `< =…>`) — the `java.io.Writer.write(char[])` symptom filed separately, see
  `java-io-writer-write-char-array-jit-miscompile-20260726.md` in this
  directory;
- two `QName`s whose local parts print identically compare unequal — the
  `UnmarshalException: unexpected element (uri:"", local:"widget")` this ban
  was kept for.

One defect, two faces. Both were attributed to the wrong layer (a `Writer`
forwarding method; a JAXB QName graph) because the symptom surfaced far from
the miscompiled read.

## Bisection evidence

Reproducer: `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`
against a real `jakarta.xml.bind`/`org.glassfish.jaxb` 4.0.7 runtime
(jaxb-runtime, jaxb-core, txw2, jakarta.xml.bind-api, istack-commons-runtime,
jakarta.activation-api).

```bash
CP=<those jars>:<probe classes dir>
<cratonvm> --java-home <jdk25> -cp "$CP" JaxbQNameProbe 4000
CRATONVM_JIT_ALLOW_PACKAGES=org/glassfish/jaxb/ <cratonvm> ... JaxbQNameProbe 4000
```

| dev commit | Writer face (default) | QName face (ban lifted + `CRATONVM_JIT_DENY=java/io/Writer`) |
|---|---|---|
| `95e4d9929` (07-26 12:15Z) | **fails i=27** — `< ="w27"><>type5_27</>…</>`| **fails i=81** — exactly the documented `UnmarshalException` |
| `82b78bca5` (07-26 13:14Z) | clean | clean |
| `13055f75c`, `55ada21db`, `c042e794f` (current dev) | clean | clean |

The 07-26 12:15Z run reproduces the previously-filed failures verbatim,
including the iteration numbers, so the "no longer reproduces" result on
current dev is a real fix and not a probe-wiring difference.

## Re-verification with the ban removed

Binary: this branch, default release profile (`lto = "fat"` — see the build
note below), real JDK 25 (`--java-home`), no synthetic stubs.

- `JaxbQNameProbe` 20000 marshal/unmarshal round trips, ban *removed from the
  source*: **0 failures, 4/4 runs**.
- Same probe on the unmodified tree with `CRATONVM_JIT_ALLOW_PACKAGES=
  org/glassfish/jaxb/`: 0 failures, 3/3 runs; ban-active control 0 failures.
- The lift is not a no-op: `CRATONVM_DBG_JIT_ENTRY=1` counts **12 914**
  `org/glassfish/jaxb/…` JIT entries over 900 probe iterations with the ban
  removed, versus **0** with it in place. 23 distinct JAXB classes/methods
  compile. The clean result is measured against real compiled JAXB code.
- Real Hibernate ORM 8.0 harness (`/data/data/apps/hibernate-orm-harness`,
  `hib-suite-runner/common.args`), 98 orm.xml / JAXB-binding /
  `bootstrap.binding.hbm` / `annotations.xml.ejb3` test classes: **266 tests
  found, 266 passed, 0 failed in BOTH configurations, and the per-class
  `@@RESULT` lines are byte-identical** between the ban-active baseline and
  the ban-removed binary.
  (`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`
  is excluded from that list — it hangs under CratonVM in both configurations,
  a pre-existing issue unrelated to this ban.)
- `cargo test -p cratonvm-vm --lib skip_list`: 68 passed, 0 failed.
  `cargo test -p cratonvm-jit --lib`: 1018 passed, 0 failed.

## Build-configuration note (methodology, worth keeping)

An earlier round of this verification used binaries built with
`CARGO_PROFILE_RELEASE_LTO=off` (a habit for faster iteration). With the ban
lifted **those binaries only** hit a separate failure:
`org.glassfish.jaxb.runtime.util.AttributesImpl.ensureCapacity` allocating a
`String[]` of 25·2^k (up to 1 677 721 600) → `OutOfMemoryError`, isolated to
that single class with `CRATONVM_JIT_BISECT_ONLY`, 3–4 runs in 4, and gone
with `CRATONVM_TIER_ENABLED=0`. The identical tree built with the project's
real release profile (`lto = "fat"`) is clean 4/4 on the exact same
configuration, as is every fat-LTO binary tested (including yesterday's dev
build, 8/8).

`lto = off` is **not** a supported build configuration for correctness
testing on this project: it changed observable JIT behaviour here. Anyone
bisecting a JIT miscompile should build with the default profile, or at
minimum confirm the failure survives a fat-LTO build before treating it as a
dev regression — it cost several bisect builds chasing a window that turned
out to be a build-flag artifact.

## Related

- `docs/internal/java-io-writer-write-char-array-jit-miscompile-20260726.md`
  — the other face of the same defect, also resolved by `82b78bca5`.
- `docs/known-issues/jit-bans/jit-ban-remaining-sweep-20260726.md` — the
  sweep that re-confirmed this ban on 2026-07-26.
- `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java` —
  the reproducer, kept: it is the regression witness for `82b78bca5`.
