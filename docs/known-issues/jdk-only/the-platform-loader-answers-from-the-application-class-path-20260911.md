# The platform loader answers from the application class path, and `getResource` disagrees with `getResourceAsStream`

*2026-09-11. Found by building the instrument the `--jdk-only`
loader-and-bootstrap lane's residual was blocked on.*

## 1. The residual this starts from

That lane closed with 20 `jdk/internal/loader/` rows **classified rather than
retired**, and the reason was not the rows:

> One of the 20 is ever dispatched by the corpus. […] That is precondition 4 of
> the retirement protocol failing for 19 of 20: *a dispatch observed by the
> instrument that produced the improvement*. […] The hierarchy links now; what
> the rows still lack is an instrument that reaches them. Naming that is the
> result.

`apps/probes/L7LoaderInternalsSweep.java` is that instrument. It drives the
loader internals through **public API only** — no `jdk.internal.loader` import,
no reflection, so no `--add-opens` or `--add-exports` — and lets the JDK reach
its own internals:

```text
  app / platform / system loader resource lookups   ClassLoaders$*, URLClassPath
  Class.getResourceAsStream on a java.base class    BootLoader.findResourceAsStream
  URLClassLoader over a directory and over a jar    URLClassPath.{Loader,JarLoader}
```

28 rows. It is padded by hand and prints with `println`, because
`System.out.printf` reaches `DecimalFormatSymbols` →
`LocaleProviderAdapter` and throws under `CRATONVM_ENFORCE_NATIVE_SHADOW=all`,
which would print zero rows and read as 28 differences.

## 2. What it measured

HotSpot 25.0.4+7 against `--jdk-only`, unarmed, 29 lines each including the
row-count line. **Exactly one row differs:**

```text
  07  platform.getResource(self)                   HotSpot null      null
  08  platform.getResourceAsStream(self)           HotSpot null      PRESENT
```

`self` is `L7LoaderInternalsSweep.class`, a resource on the **application class
path**. The platform loader must not find it, and HotSpot does not.

The interesting half is not row 08 alone, it is **07 and 08 together**. The
JDK defines one in terms of the other:

```java
public InputStream getResourceAsStream(String name) {
    URL url = getResource(name);
    return url != null ? url.openStream() : null;
}
```

So on one loader, for one name, in one run, these two answers cannot disagree —
and here they do. That makes row 08 a self-evidencing defect: it needs no oracle
to be wrong, because row 07 is its oracle.

## 3. The cause is one registration loop

`native-builtins/src/classloader.rs`:

```rust
r.register(cl, "getResourceAsStream", "(…)Ljava/io/InputStream;",
           cl_get_resource_as_stream);
…
for builtin_cl in [
    "jdk/internal/loader/BuiltinClassLoader",
    "jdk/internal/loader/ClassLoaders$AppClassLoader",
    "jdk/internal/loader/ClassLoaders$PlatformClassLoader",
] {
    r.register(builtin_cl, "getResourceAsStream", "(…)Ljava/io/InputStream;",
               cl_get_resource_as_stream);
}
```

One callback for every loader shape — `java.lang.ClassLoader`,
`URLClassLoader`, and all three built-in loaders — so the answer does not depend
on which loader was asked. `getResource` is registered for `cl` and
`URLClassLoader` **and not for the three built-in loaders**, so on the platform
loader it runs the real bytecode and answers correctly. That asymmetry in the
registrar is exactly the asymmetry the probe sees.

Three of the lane's 19 unreachable rows are these registrations. They are now
reachable, and one of them is measurably wrong.

## 4. No arm of the dial can reach it

```text
  CRATONVM_ENFORCE_NATIVE_SHADOW=jdk/internal/loader/ClassLoaders$PlatformClassLoader
  CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/ClassLoader
  CRATONVM_ENFORCE_NATIVE_SHADOW=all
      row 08 = present, in all three
```

This is the lane's own headline finding in a second place. The dial yields a
native to the bytecode **of the class the native is registered on**, and these
are bucket B: `ClassLoaders$PlatformClassLoader` does not declare
`getResourceAsStream`, it inherits it. There is no `Code` on the receiver class
to yield to, so the widest possible scope changes nothing. Only a
registration-time lever reaches these rows — the same conclusion the lane
reached for `SecureClassLoader.<clinit>`, arrived at from the opposite
direction.

## 5. Why this is recorded and not fixed here

The obvious fix — delete the three built-in-loader registrations so the
inherited bytecode runs — **is not sufficient on its own.** `cl_get_resource_as_stream`
is still registered on `java/lang/ClassLoader` itself, and a dispatch door asks
the registry about the declaring class, so the platform loader would reach the
same callback by the same wrong answer with one fewer registration in the way.

The sufficient fix is in `cl_get_resource_as_stream`: its search must depend on
the receiver's loader identity, so that the platform loader sees platform
modules and not the application class path. That is a change to the resource
resolution path every vector in the corpus depends on, and it needs its own
before/after across the whole corpus rather than a one-probe verdict. Retiring
the rows instead is **not** the alternative it looks like: a refused
`SyntheticStub` under `--jdk-only` falls through to an older native rather than
to the bytecode, so a retirement here trades a wrong answer for a different
wrong answer.

What is established, and is what the residual asked for:

* an instrument that reaches the rows, in the tree, runnable by the next lane;
* a dispatch observed on three of the 19, satisfying precondition 4 for them;
* a defect those rows cause, self-evidencing via row 07;
* the lever identified as registration-time, with the dial excluded by
  measurement at three scopes rather than by argument.

## 6. Also seen, and not yet priced

`getResources` on every loader shape drives the VM to request
`java/util/Enumeration$Impl`, which `--jdk-only` refuses to fabricate:

```text
  refusing to fabricate a compatibility stand-in for this class …
  class="java/util/Enumeration$Impl" requested_by="native-builtins/src/classloader.rs:5611"
  …:5639  …:6514
```

Rows 04, 13, 20 and 25 (the four `getResources` counts) **match HotSpot anyway**,
so this is a §1.4 fabrication that is currently benign in answer terms — which is
the reason to record it rather than the reason to ignore it. Three call sites,
named above.
