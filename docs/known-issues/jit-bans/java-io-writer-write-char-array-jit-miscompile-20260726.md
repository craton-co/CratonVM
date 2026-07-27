# `java.io.Writer.write(char[])` JIT miscompile — silently drops written characters (NEW, undiscovered)

**Status: OPEN.** Found while re-testing the `org/glassfish/jaxb/` JIT ban
(`jaxb_mapping_residual_skip_prefix`) as part of the JIT-ban-removal sweep
following the 2026-07-26 JIT rework. This is a **general VM correctness bug**,
not specific to JAXB or any app — it can corrupt any write through
`Writer.write(char[])`'s default one-argument overload once the calling
method is hot enough to JIT.

## Symptom

A `StringWriter`-backed writer silently drops a whole `write(char[])` call's
content partway through a run. Standalone repro: marshal a small
`@XmlRootElement`-annotated class via a real `jakarta.xml.bind`/
`org.glassfish.jaxb` `Marshaller` into a `StringWriter`, in a loop, with a
fresh `Marshaller`/`StringWriter` each iteration. From iteration 26 onward
(deterministic, every run), the produced XML has an **empty closing tag**:

```
<?xml version="1.0" encoding="UTF-8" standalone="yes"?><widget id="w26"><type>type4_26</type><refs>ref26-0</refs><refs>ref26-1</refs><refs>ref26-2</refs></>
```

Expected `</widget>`; got `</>` — the root element name (6 ASCII characters,
"widget") vanished from between `</` and `>`. Every other write in the same
document (opening tags, attribute values, nested element text) is correct;
only this one write call, always at the same iteration, is affected in a
20-run/20-run reproduction.

## Root cause (bisected, not yet fixed)

Confirmed **completely unrelated to JAXB or the existing `org/glassfish/jaxb/`
ban**: reproduces identically whether that ban is active or lifted via
`CRATONVM_JIT_ALLOW_PACKAGES=org/glassfish/jaxb/`. Bisected with
`CRATONVM_JIT_DENY` / `CRATONVM_JIT_BISECT_SKIP` / `CRATONVM_JIT_BISECT_ONLY`
(no rebuild needed once a diagnostic binary exists):

- `CRATONVM_JIT_DENY=java/` (force all JDK classes to interpret) → clean.
- `CRATONVM_JIT_DENY=java/io/` → clean. `CRATONVM_JIT_DENY=java/io/Writer` (a
  substring match, so it also covers `java/io/Writer` itself but NOT
  `java/io/StringWriter`, a different string) → **still fails** — see below,
  this pins it to `Writer` itself, not `StringWriter`.
- `CRATONVM_JIT_DENY=java/lang/`, `java/util/`, `java/lang/reflect/` → each
  individually **still fails** (i.e. not those packages).
- `CRATONVM_JIT_BISECT_SKIP=java/io/Writer.write` → clean.
  `CRATONVM_JIT_BISECT_SKIP=java/io/Writer.append` / `.close` / `.flush` →
  each **still fails**. Pins the defect to one of `Writer`'s `write(...)`
  overloads specifically (bisect-skip matches by method name, not exact
  overload, so this narrows to the `write` family, not a specific arity yet).
- `CRATONVM_JIT_BISECT_ONLY=java/io/Writer` (ONLY this class JIT-eligible,
  everything else forced to interpret) → **still fails**, deterministically
  at the same iteration 26. This is the cleanest isolation: the defect is
  fully self-contained within `java.io.Writer`'s own bytecode, not an
  interaction with any other hot class.
- `--nojit` (interpreter only) → clean, 100/100 and 1000/1000 in separate
  runs. Confirms JIT-specific, not a general JAXB/reflection bug.

`java.io.Writer.write(char[] cbuf)` (real JDK 25, no CratonVM native stub) is
a one-line forwarding method:
```java
public void write(char[] cbuf) throws IOException {
    write(cbuf, 0, cbuf.length);
}
```
`StringWriter` does not override this particular zero-argument-array overload
(it overrides `write(int)`, `write(char[], int, int)`, `write(String)`,
`write(String, int, int)`, and the `append(...)` family), so any caller that
invokes `Writer.write(char[])` on a `StringWriter` receiver executes this
exact inherited one-liner. The most likely failure mode given the symptom
(the whole write silently no-ops — no exception, no partial content, just
zero characters appended) is that the virtual dispatch or the `cbuf.length`
read in the trivial forwarding call is getting corrupted to `0` under JIT,
turning `write(cbuf, 0, cbuf.length)` into `write(cbuf, 0, 0)`. This has not
been confirmed at the disassembly level — no fix has been attempted.

## Reproducer

`JaxbQNameProbe.java` (marshal/unmarshal loop over a `QName`-bearing
`@XmlRootElement` class using a real `jakarta.xml.bind`/`org.glassfish.jaxb`
4.0.7 runtime + a plain `java.io.StringWriter`), run under default JIT:
fails deterministically at iteration 26 of any run ≥27 iterations. Under
`--nojit`, or with `CRATONVM_JIT_BISECT_SKIP=java/io/Writer.write`, or with
`CRATONVM_JIT_DENY=java/io/Writer`: clean.

```bash
CP=<jaxb-runtime,jaxb-core,txw2,jakarta.xml.bind-api,istack-commons-runtime,jakarta.activation-api jars>:<probe classes dir>
<cratonvm-binary> --java-home <jdk25> -cp "$CP" JaxbQNameProbe 100
# fails at i=26 with a malformed "</>" closing tag, then an UnmarshalException
# parsing that malformed XML back.

<cratonvm-binary> --java-home <jdk25> --nojit -cp "$CP" JaxbQNameProbe 100
# clean, 100/100.
```

## Blast radius

Any real application that writes through `Writer.write(char[])` on a
receiver that doesn't override that specific overload — not limited to
`StringWriter`; any `Writer` subclass that overrides only the `(char[], int,
int)` / `String` / `append` family and relies on the inherited `write(char[])`
forwarder is equally exposed once that call site gets hot enough to compile.
This is likely a **wider-reaching bug than the JAXB ban it was found under**;
worth checking whether other already-catalogued "mysterious partial/dropped
output" bugs in this codebase's history are actually this same root cause.

## Relationship to the JAXB ban

**Does not explain and does not subsume** `jaxb_mapping_residual_skip_prefix`
(`org/glassfish/jaxb/`). That ban is confirmed independently still necessary:
with this `Writer` bug worked around (`CRATONVM_JIT_DENY=java/io/Writer`) and
the `org/glassfish/jaxb/` ban ALSO lifted, the same probe hits a **different**
JIT-only corruption at iteration 81 — an `UnmarshalException: unexpected
element (uri:"", local:"widget")` where the parsed/expected `QName` values
print as textually identical but compare unequal (matching the ban's own
documented "QName-heavy runtime graph" / self-cast corruption symptom). With
the `org/glassfish/jaxb/` ban kept active (default) and only the `Writer` bug
worked around, the same 4000-iteration probe is clean. See test log summary:

| Config | Result |
|---|---|
| Default (jaxb ban active, Writer bug NOT worked around) | Fails at i=26 — this `Writer` bug |
| Jaxb ban active + `CRATONVM_JIT_DENY=java/io/Writer` | Clean, 4000/4000 |
| Jaxb ban LIFTED + `CRATONVM_JIT_DENY=java/io/Writer` | Fails at i=81 — separate QName corruption, confirms jaxb ban still needed |
| Jaxb ban LIFTED + `--nojit` | Clean, 1000/1000 |

**Action for whoever picks this up:** root-cause `Writer.write(char[])`'s
codegen (start with `CRATONVM_JIT_BISECT_ONLY=java/io/Writer` +
`CRATONVM_FRAME_TRACE=1`/disassembly on the compiled method — no rebuild
needed to reproduce). Once fixed, re-run this exact probe to confirm, and
separately leave `jaxb_mapping_residual_skip_prefix` alone (it guards a
different, still-live bug).
