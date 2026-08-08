# JDK-Only Mode

`--jdk-only` selects a **policy**, not a class library. It keeps the real JDK
that real-JDK mode already boots, and forbids the compatibility substitutions
CratonVM would otherwise make to keep a program moving.

> **Status: internal diagnostic (stage 1 of 4).** Stage 1 is instrumentation and
> measurement; enforcement is partial. A program that runs fine under
> `--real-jdk` may fail under `--jdk-only` — that is the signal the mode exists
> to produce, not a bug in the program. This is not yet a compatibility
> guarantee.

## What it means

Real class bytes are authoritative:

- a real JDK runtime image is **required** — there is no silent fallback;
- no non-array class is fabricated: a class with no real bytes raises the
  specification-appropriate `ClassNotFoundException` / `NoClassDefFoundError`
  instead of getting a stand-in;
- no synthetic-stub native is registered or invoked;
- concrete Java bytecode beats any registered native, except a reviewed
  intrinsic;
- an `ACC_NATIVE` method binds to a bridge or a reviewed intrinsic, or fails
  with a structured `MissingNative` error — never a stub.

Class fabrication and stub registration enforce today. The dispatch half is
measured rather than refused on several paths, and some warm and compiled
dispatch paths are neither counted nor checked yet. Which is which is tracked
in the [migration guide](../../../jdk-only-migration.md).

Note that "synthetic" is overloaded. A class file's `ACC_SYNTHETIC` flag has
nothing to do with this mode; what `--jdk-only` forbids is a *compatibility
substitution* — a fabricated class, or a `SyntheticStub` native.

## Flags

| Flag | Effect |
|------|--------|
| `--jdk-only` | Real JDK, compatibility substitutions rejected. |
| `--jdk-only-report <FILE>` | Violation/counter report as JSON on shutdown. |
| `--dump-class-origins <FILE>` | Class-origin census as JSON on shutdown. |
| `--trace-jdk-only` | Report each recorded violation to stderr and at `WARN`. |
| `--explain-jdk-only` | Long-form explanation per violation; also keeps absolute paths in the report files. |

`--jdk-only` **implies `--real-jdk`**: it tightens which substitutions are
permitted, it does not choose a class library. It **conflicts with
`--synthetic-jdk`**, and the launcher says so specifically rather than raising
a generic library-selection conflict — the synthetic library is built entirely
out of the substitutions this mode exists to forbid, so a strict run would have
no class library left to execute.

The four diagnostic flags work **with or without** `--jdk-only`. Under the
default compatibility policy the recorded violations are the ones a strict run
*would* have hit, which is what makes the files a measurement of the distance to
strict mode rather than a post-mortem of a failed one.

`--trace-jdk-only` is a *polling* trace: the launcher drains the append-only
violation logs immediately after VM construction (where every registration
refusal is produced) and again at shutdown. Class-origin violations recorded
mid-run therefore surface at shutdown rather than as they happen.

`--explain-jdk-only` disables the path redaction applied to
`--jdk-only-report`, `--dump-class-origins` and `--dump-native-registry`. Leave
it off for anything committed as a baseline or attached to a bug report.

`CRATONVM_REAL=-stubs` (equivalently `CRATONVM_NO_STUBS`) still works as a
native-registry filter, but it expresses only the registry third of this
contract — classes with no real bytes are still fabricated and a registered
native still wins over concrete bytecode. Setting it without `--jdk-only`
prints a one-time note saying so.

## Origins: generated is not fabricated

`--dump-class-origins` records how every loaded class came to exist. Reading it
correctly depends on one distinction that is easy to get backwards: **arrays,
hidden classes, lambdas, proxies and reflection accessors are legitimate
origins, not compatibility stubs.** A conforming JVM creates all of them
without any class file, so they carry their own distinct origin and are
*allowed* under `--jdk-only`. Exactly one origin is forbidden.

| `origin` tag | Meaning | `--jdk-only` |
|---|---|---|
| `boot-image` | Loaded from the JDK runtime image (`jmods/` or `lib/modules`). | allowed |
| `application-class-path` | Loaded from the classpath or a JAR. | allowed |
| `user-defined` | Defined by a user class loader. | allowed |
| `vm-array` | An array class, created by the VM. | allowed |
| `hidden-class` | A hidden class (`Lookup::defineHiddenClass`). | allowed |
| `generated-lambda` | A lambda body spun at an `invokedynamic` call site. | allowed |
| `generated-proxy` | A `java.lang.reflect.Proxy` implementation class. | allowed |
| `reflection-accessor` | A generated reflection accessor. | allowed |
| `vm-internal` | A VM-internal class with no user-visible bytes. | allowed |
| `compatibility-stub` | **Fabricated**: no real bytes exist for it anywhere. | **forbidden** |

Only `compatibility-stub` rows carry a `reason`. That is the bucket a strict
run requires to be empty, and the number to watch release over release; the
generated origins above are counted separately precisely so they are not
mistaken for it.

Each census row also carries `real_bytes_found`, `requested_by` and the
`loader_id`, and the file is sorted so it is byte-stable and diffable against a
committed baseline.

## Measuring before enforcing

The usual first run changes nothing about how the program executes:

```bash
cratonvm --dump-class-origins origins.json \
         --dump-native-registry natives.json \
         --jdk-only-report jdk-only.json \
         --classpath . MyApp
```

`jdk-only.json` lists the violations and folds the origins onto four class
counters — `boot_image_classes`, `application_classes`, `generated_classes`
(the five generated origins above) and `compatibility_classes` — alongside
per-kind native dispatch counts. `user-defined` and `vm-internal` fall into no
bucket, so read `origins.json` for the full per-origin breakdown. See
[Debugging & Diagnostics](debugging.md) and
[Observability](../operations/observability.md).

Then, to see what strict mode would actually refuse:

```bash
cratonvm --jdk-only --trace-jdk-only --classpath . MyApp
```

## Falling back

`--real-jdk` is the fallback, and it is the default: same binary, same JDK,
compatibility substitutions permitted again. `--synthetic-jdk` is not a
fallback — it is a different class library, and it conflicts with `--jdk-only`.

## Further reading

- [`docs/jdk-only-migration.md`](../../../jdk-only-migration.md) — the operator
  and migration guide: flag matrix, reading a failure, the two-run triage,
  violation kinds, and the rollout stages.
- [`docs/feature-designs/jdk-only-mode.md`](../../../feature-designs/jdk-only-mode.md)
  — the normative design and cross-crate contract.
- [JDK Modes: Real vs. Synthetic](../getting-started/jdk-modes.md) — which
  class library boots, as opposed to which substitutions are permitted.
