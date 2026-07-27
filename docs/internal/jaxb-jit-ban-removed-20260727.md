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
full Hibernate mapping-metadata bootstrap — was not re-run here (Hibernate is
independently kept interpreted by the still-active HIB-TEMPORAL.1 `org/hibernate/`
ban, so JAXB's graph is not driven from that path under the default policy
anyway). If a QName self-cast resurfaces from a Hibernate boot, re-open with
that as the repro rather than this probe.

## Related

- `docs/internal/jit-licm-preheader-bypass-20260727.md`
- `docs/internal/java-io-writer-write-char-array-jit-miscompile-20260726.md`
- `docs/internal/jit-ban-remaining-sweep-20260726.md` — the sweep this came from
- `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`
