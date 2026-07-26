# JAXB (`org/glassfish/jaxb/`, `jaxb_mapping_residual_skip_prefix`) — CONFIRMED still needed

**Status: ban kept, re-verified live with a real JIT-only correctness bug.**
Part of the "remove all app-specific JIT bans" sweep following the
2026-07-26 JIT rework (`docs/internal/jit-ban-remaining-sweep-20260726.md`).

## Repro

Real `jakarta.xml.bind`/`org.glassfish.jaxb` 4.0.7 runtime (no synthetic
stub), no ES/Hibernate context needed — self-contained. `Widget` class with
a `@XmlElement` `QName` field and a `List<QName>` field
(`docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`):
each iteration builds a fresh `Widget`, marshals it into a `StringWriter`
via a fresh `Marshaller`, then unmarshals the resulting XML back via a
fresh `Unmarshaller`, and checks the round-tripped `QName` values compare
equal.

```bash
CP=<jaxb-runtime,jaxb-core,txw2,jakarta.xml.bind-api,istack-commons-runtime,jakarta.activation-api jars>:<probe classes dir>
<cratonvm-binary> --java-home <jdk25> -cp "$CP" JaxbQNameProbe 4000
```

## Result

Testing this ban in isolation required first working around an unrelated,
newly-discovered general VM bug
(`docs/known-issues/java-io-writer-write-char-array-jit-miscompile-20260726.md`
— `java.io.Writer.write(char[])` silently drops output once JIT-compiled,
which corrupts the SAME probe's XML output for a completely different
reason starting at iteration 26, before the JAXB-specific code path is
ever reached). All four configs below use
`CRATONVM_JIT_DENY=java/io/Writer` to hold that separate bug fixed/absent
so the JAXB-specific behavior can be isolated:

| Config | Result |
|---|---|
| Ban active (default) + Writer-bug workaround | Clean, 4000/4000, no failures |
| Ban **lifted** (`CRATONVM_JIT_ALLOW_PACKAGES=org/glassfish/jaxb/`) + Writer-bug workaround | **Fails at iteration 81/4000**: `jakarta.xml.bind.UnmarshalException: unexpected element (uri:"", local:"widget"). Expected elements are <{}widget>` — the parsed element's local name and the expected local name print identically ("widget") yet the runtime treats them as unequal. Matches this ban's own documented "QName-heavy runtime graph" / self-cast `QName cannot be cast to QName`-class corruption. |
| Ban lifted + `--nojit` | Clean, 1000/1000 — confirms the failure above is JIT-specific, not a general regression from lifting the ban under any execution mode. |

Confirmed JIT-specific and confirmed independent of the `Writer` bug (the
`Writer` bug reproduces with the JAXB ban either active or lifted; this
QName corruption reproduces ONLY when the JAXB ban is lifted, regardless of
the `Writer` workaround).

## Disposition

**KEEP `org/glassfish/jaxb/` banned.** Real, live, JIT-only correctness bug,
independently reproduced from the original 2026-07-18-era report. Not yet
root-caused past "QName equality/identity corrupted somewhere in the
glassfish JAXB runtime under JIT" — the original ban's own comment already
narrows the symptom to "QName-heavy runtime graph"; this session's repro
adds a fresh, minimal, fully standalone reproducer that doesn't depend on
Hibernate mapping metadata or any app framework, useful for whoever
root-causes the actual corrupted state next.

## Related

- `docs/internal/jit-ban-remaining-sweep-20260726.md` — this session's
  tracking doc for the "remaining unclaimed candidates" batch.
- `docs/known-issues/java-io-writer-write-char-array-jit-miscompile-20260726.md`
  — the separate new bug found while testing this one; unrelated, does not
  explain or subsume this ban.
- `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`
  — the reproducer used above.
