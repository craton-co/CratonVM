# JAXB (`org/glassfish/jaxb/`, `jaxb_mapping_residual_skip_prefix`) — BAN REMOVED 2026-07-27

**Status: ✅ ban removed from `vm/src/jit/skip_list.rs`.** Supersedes
`docs/known-issues/jit-bans/jaxb-still-needed-20260726.md` ("CONFIRMED still
needed", 2026-07-26), which is what this file was.

## What the ban was for

A self-cast `QName cannot be cast to QName` observed while Hibernate mapping
metadata built JAXB's QName-heavy runtime graph under JIT. On 2026-07-26 it was
re-confirmed with a standalone probe
(`docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java` — real
`jakarta.xml.bind` / `org.glassfish.jaxb` 4.0.7, a `Widget` with a `QName` field
and a `List<QName>` field, fresh `Marshaller` + `StringWriter` + `Unmarshaller`
per iteration) as:

> `jakarta.xml.bind.UnmarshalException: unexpected element (uri:"",
> local:"widget"). Expected elements are <{}widget>` at iteration **81** of
> 4000, with the parsed and expected local names printing identically.

That 2026-07-26 run used `CRATONVM_JIT_DENY=java/io/Writer` to hold a separate
bug out of the way (see
`docs/internal/java-io-writer-write-char-array-jit-miscompile-20260726.md`).

## Re-verification, 2026-07-27

Dev at `c042e794f`, real JDK 25 (`--java-home`), same probe, same jars.

| Binary | Config | Result |
|---|---|---|
| dev + build fix only (pre-LICM-fix) | ban **lifted** via `CRATONVM_JIT_ALLOW_PACKAGES` + `CRATONVM_JIT_DENY=java/io/Writer` — the exact 2026-07-26 config | **clean, 4000/4000** |
| dev + build fix only | ban lifted, no `Writer` workaround, 4000 | dies with `OutOfMemoryError … anewarray … length 1677721600` in `SAXOutput.attribute` |
| **final** (LICM fix + ban deleted from the skip list) | 4000 iterations × 6 runs | **clean, 0 failures** |

Two independent conclusions:

1. **The QName corruption is gone.** It does not reproduce even on the
   pre-fix binary under the exact configuration that produced it a day earlier,
   so it was closed by general JIT work merged into dev between 2026-07-26 and
   2026-07-27 — not by this session's change. Nothing in this session touched
   QName handling.
2. **What the 2026-07-26 session was actually still hitting at 4000 iterations
   was a different, general bug** — the LICM / speculative pre-header bypass
   (`docs/internal/jit-licm-preheader-bypass-20260727.md`), reached through
   `org.glassfish.jaxb…SAXOutput.attribute` →
   `org.xml.sax.helpers.AttributesImpl.addAttribute` → `ensureCapacity`.
   `AttributesImpl` is a **JDK** class, so that failure has nothing to do with
   the `org/glassfish/jaxb/` package being JIT-eligible; it reproduces
   standalone with the ban fully active. It is now fixed.

## Disposition

`jaxb_mapping_residual_skip_prefix` and its `should_skip_jit_internal` call site
are **deleted**; the unit test
`jaxb_mapping_package_skipped_conservatively_and_lifts_for_bisection` is
replaced by `jaxb_mapping_package_is_jit_eligible_after_removal`, which asserts
the package is now JIT-eligible under `SkipPolicy::Conservative`.

**Caveat, stated plainly:** the evidence above is the standalone
`JaxbQNameProbe`, which is the witness the 2026-07-26 session itself nominated
and the only reproducer this ban has. The *original* 2026-07-18-era context — a
full Hibernate mapping-metadata bootstrap — was not re-run here (Hibernate was
independently kept interpreted by the then-active HIB-TEMPORAL.1
`org/hibernate/` ban, so JAXB's graph was not driven from that path under the
default policy anyway). HIB-TEMPORAL.1 was fixed and removed on 2026-07-29. If
a QName self-cast resurfaces from a Hibernate boot, re-open with that as the
repro rather than this probe.

## Related

- `docs/internal/jit-licm-preheader-bypass-20260727.md`
- `docs/internal/java-io-writer-write-char-array-jit-miscompile-20260726.md`
- `docs/internal/jit-ban-remaining-sweep-20260726.md` — the sweep this came from
- `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`

---

## Follow-up, same day, second session: root cause pinned, removal completed, Hibernate caveat closed

Three additions to the above, from an independent session working the same
doc. Nothing here contradicts it; each item fills in something it left open.

### 1. The removal was only half-applied

`jaxb_mapping_residual_skip_prefix` (`vm/src/jit/skip_list.rs`) is the VM-side
eligibility gate. There is a **second** gate: `jaxb_mapping_jit_deny_prefix` in
`jit/src/lib.rs`, consulted inside `try_compile` itself so background/tiered
compiles fail closed too. It was still present. With it in place `try_compile`
returns `None` for every `org/glassfish/jaxb/…` method regardless of the skip
list, so a "ban removed" verification run that does not also pass
`CRATONVM_JIT_ALLOW_PACKAGES` compiles no JAXB code at all and its clean result
is vacuous — the same shadowing trap as the `SPRINGBOOT-WITHOUT-JACKSON.2`
finding in the 2026-07-26 sweep.

Measured with `CRATONVM_DBG_JIT_ENTRY=1` over 900 `JaxbQNameProbe` iterations,
no env overrides:

| tree | `org/glassfish/jaxb/…` JIT entries |
|---|---:|
| both gates present | 0 |
| **both gates removed** | **12 914** (23 distinct classes/methods) |

Both gates and both unit tests are now deleted.

### 2. The QName corruption has an exact fixing commit

The doc above concludes the corruption "was closed by general JIT work merged
into dev between 2026-07-26 and 2026-07-27". Bisected, with the ban's own
reproducer, to a single commit:

| dev commit | `JaxbQNameProbe`, ban lifted + `CRATONVM_JIT_DENY=java/io/Writer` |
|---|---|
| `95e4d9929` (07-26 12:15Z) | **fails at iteration 81** — `UnmarshalException: unexpected element (uri:"", local:"widget")`, verbatim |
| `82b78bca5` (07-26 13:14Z) | clean |
| `13055f75c`, `55ada21db`, `c042e794f`, current dev | clean |

`82b78bca5` is **"fix(jit): String field intrinsics read compact primitive
fields 4 bytes high"**. The corruption was never JAXB-specific: JAXB's
marshaller writes element and attribute names as interned `String`s, and the
compact-layout `String` intrinsic read the primitive `coder`/`hash` field four
bytes above its real offset, so those names decoded empty or garbage. The same
commit closes the `java.io.Writer.write(char[])` report filed alongside it —
the "vanished element name" and the "two QNames print alike but compare
unequal" symptoms are one defect seen from two directions.

The 12:15Z binary reproducing the documented failure **at the documented
iteration number** is what makes the later clean runs trustworthy rather than a
probe-wiring difference.

### 3. The Hibernate caveat is closed

The caveat above — that the original 2026-07-18-era context (a full Hibernate
mapping-metadata bootstrap) was not re-run — no longer applies. Real Hibernate
ORM 8.0 harness (`/data/data/apps/hibernate-orm-harness`,
`hib-suite-runner/common.args`), 98 orm.xml / JAXB-binding /
`bootstrap.binding.hbm` / `annotations.xml.ejb3` test classes, run twice:

| config | classes | tests found | passed | failed |
|---|---:|---:|---:|---:|
| ban active (baseline) | 98 | 266 | 266 | 0 |
| **both gates removed** | 98 | 266 | 266 | 0 |

The per-class `@@RESULT` lines are **byte-identical** between the two runs.
(`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`
is excluded from the list — it hangs under CratonVM in *both* configurations, a
pre-existing issue unrelated to this ban.)

Plus, on the ban-removed binary: `JaxbQNameProbe` at **20000** iterations, 0
failures, 4/4 runs; `cargo test -p cratonvm-vm --lib skip_list` 68 passed;
`cargo test -p cratonvm-jit --lib` 1018 passed.

### Build-configuration note

The `AttributesImpl.ensureCapacity` runaway allocation that the LICM
pre-header-bypass fix addresses (`jit-licm-preheader-bypass-20260727.md`)
reproduced here **only in binaries built with
`CARGO_PROFILE_RELEASE_LTO=off`** — 4/4 runs, and isolated to a single class
with `CRATONVM_JIT_BISECT_ONLY=org/glassfish/jaxb/runtime/util/AttributesImpl`
(the JAXB copy, not only the `org.xml.sax.helpers` one). The identical tree
built with the project's real release profile (`lto = "fat"`) was clean 4/4 on
the same configuration, and clean at 20000 iterations. `lto = off` is a
convenient iteration shortcut but is **not** a sound configuration for
correctness testing on this project — it both hid and exposed real behaviour
here, and cost several bisect builds chasing a window that turned out to be a
build-flag artifact. Bisect JIT miscompiles with the default profile.
