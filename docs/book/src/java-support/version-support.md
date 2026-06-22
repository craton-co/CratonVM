# Java Version Support

CratonVM accepts class files from **Java SE 8 through 25** (class-file major
versions up to 69) and implements language and runtime features across that
range.

| Version | Representative features supported |
|---------|-----------------------------------|
| **Java 8** | Lambdas, method references, streams, default methods, `java.time` |
| **Java 11** | Nest-based access control (JEP 181), `var` in lambdas, new `String`/`Collection` APIs |
| **Java 17** | Records (JEP 395), sealed classes (JEP 409), pattern matching for `instanceof` |
| **Java 21** | Pattern matching for `switch`, record patterns, virtual threads, sequenced collections |
| **Java 25** | Stream gatherers, scoped values, structured concurrency, class-file version 69 |

These are highlights, not an exhaustive conformance list. CratonVM is not a
certified Java SE implementation, and individual newer APIs may be partial or
unimplemented — see [Known Limitations](limitations.md) and the [Standard
Library Coverage](standard-library.md) page.

## Choosing a class-file target

Compile your sources for any target in the 8–25 range:

```bash
# Target a specific release
javac --release 17 MyApp.java

# Or just use a modern JDK's default
javac MyApp.java
```

CratonVM reads the class-file version and runs the bytecode accordingly. If you
hit a feature that isn't implemented, the [missing-natives
audit](../user-guide/debugging.md#finding-missing-standard-library-methods) and
`--verbose:class` are the quickest way to see what's involved.

## Bytecode verification

CratonVM verifies bytecode by default (`--Xverify remote`, matching HotSpot:
non-boot classes are verified). Modern class files carry `StackMapTable`
attributes and verify with the type-checking verifier. Very old (pre-Java-7)
class files that rely on the legacy split verifier are an area of ongoing
completeness work; if you must run such a class and verification rejects it, you
can fall back to `--noverify` (not recommended).
