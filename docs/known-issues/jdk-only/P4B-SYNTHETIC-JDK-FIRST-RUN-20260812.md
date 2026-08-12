# `--synthetic-jdk` MODE: the first run, and what it says

**2026-08-12. A `--features synthetic-jdk` release binary was built from clean
`HEAD` (`git archive`, separate target dir) and the runtime mode exercised.**
Per `P4B-SYNTHETIC-JDK-MODE-20260812.md`, a shipping binary refuses this mode
outright (exit 1) — the mode requires the feature build — which is why the
campaign's residuals that live only here had never been adjudicated.

## The numbers

Same probes, three configurations, HotSpot 25 as oracle:

| probe | HotSpot | `--jdk-only` | `--synthetic-jdk` |
|---|---|---|---|
| `P1Witness` (8) | 8 / 0 | 2 / 6 | **7 / 1** |
| `FabReach` (17) | 17 / 0 | 15 / 2 | **12 / 5** |
| `FabReach2` (16) | 16 / 0 | 15 / 1 | **5 / 11** |
| `FabReach3` (7) | 7 / 0 | 3 / 4 | **died — `NoSuchMethodError`, no `CK` line** |

Two results worth stating plainly:

* **Synthetic mode is far weaker than strict mode on ordinary Java**, which is
  the expected shape — it has no real class library — but it is now measured
  rather than assumed. `FabReach2` at 5/16 is the sharpest: `System.getLogger`,
  FFM, `WatchService`, `FileChannel`, `HttpServer`, JMX, `RandomAccessFile`,
  `HttpCookie`, `URI`, `JarFile` all fail.
* **The two modes fail on disjoint sets in at least one place.**
  `Predicate.and/or/negate` **passes** under `--synthetic-jdk` and **fails**
  under `--jdk-only`; `P1-A`'s `java.sql` loads fine here and is fatal there.
  So neither mode's result predicts the other's, and a residual "reproduced in
  Compatible" says nothing about strict, or vice versa.

## The finding: a `NoSuchMethodError` that names the wrong class

Several failures name a class that does not declare the method at all:

```
Collections.list(...)      -> NoSuchMethodError: cratonvm.internal.UnmodifiableList.enumeration(Ljava/util/Collection;)…
HttpCookie.parse("a=b")    -> NoSuchMethodError: java.lang.String.parse(Ljava/lang/String;)Ljava/util/List;
Files.newOutputStream(p)   -> NoSuchMethodError: java.nio.file.Path.newOutputStream(Ljava/nio/file/Path;…)
```

In each case **the class named is the runtime class of argument 0**, not the
class declaring the static method. `Collections.enumeration` is reported on
`UnmodifiableList`, `HttpCookie.parse` on `String`, `Files.newOutputStream` on
`Path`.

**Do not read this as a systematic static-dispatch defect — I nearly did.** A
direct test refutes the general form:

```java
Collections.enumeration(new ArrayList<>());  // FAIL -> java.util.ArrayList.enumeration(...)
Arrays.asList("a","b");                      // OK
String.valueOf(42);                          // OK
Integer.toHexString(255);                    // OK
Objects.requireNonNull("x");                 // OK
```

Four static calls whose first argument is of an unrelated class all dispatch
correctly. So static dispatch is **not** generally broken. The misnaming
appears only where the method is genuinely **absent from the synthetic class
library** — `Collections.enumeration` is simply not implemented here.

So there are two defects, one large and one small, and they must not be
conflated:

1. **Missing methods** — the real gap. `Collections.enumeration`,
   `ArrayDeque.<init>(Collection)`, `String.contentEquals(CharSequence)`,
   `ServerSocket.<init>(int,int,InetAddress)`, `RandomAccessFile.<init>(File,String)`,
   and the `StandardOpenOption`/`StandardWatchEventKinds`/`ValueLayout`/
   `System$Logger$Level` **static fields** (`NoSuchFieldError`, a different
   shape again).
2. **The error names the wrong class** when resolution fails, sending a reader
   to a class that never had the method. Cosmetic against a crash, expensive
   against a debugging session — and it is exactly what made the first reading
   of this data look like a dispatch bug.

## One genuine wrong answer, not a missing method

```
Function.identity/andThen/compose
  -> ClassCastException: class java.lang.String cannot be cast to class java.lang.Integer
```

This is not an absence. The composite `Function` exists and computes the
**wrong** result — `identity().andThen(String::length)` is applying the two in
the wrong order, or the identity is not identity. Note the inversion against
strict mode, where `Function.*` is the family that **works** and
`Predicate`/`Consumer` are broken. Same file, opposite verdicts per mode.

## What this settles

* **Residuals living only in `--synthetic-jdk` can now be adjudicated** — the
  binary exists, at `/c/craton/synjdk-target/release/cratonvm.exe`, built from
  clean `HEAD`. Several records have carried such residuals for weeks.
* **The mode is not close to running applications**, and nothing in the
  campaign depends on it being so. Its value is as the configuration in which
  ~5,200 otherwise-dead synthetic stubs are reachable at all.
* **A record that says "unreproducible" about a synthetic-mode residual
  measured the wrong binary** — and a record that reproduces one here has
  learned nothing about the shipping modes, because the failure sets are
  disjoint.

## Reproduce

```
git archive HEAD | tar -x -C <clean-dir>
cd <clean-dir> && CARGO_TARGET_DIR=<t> CARGO_PROFILE_RELEASE_LTO=off \
    cargo build --release -p cratonvm-cli --features synthetic-jdk
<t>/release/cratonvm.exe --synthetic-jdk -cp <probes> FabReach
```

Build from `git archive`, not the working tree: a multi-agent campaign leaves
the tree half-edited and a binary built from it measures nothing reproducible.
