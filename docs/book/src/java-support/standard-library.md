# Standard Library Coverage

CratonVM supplies the standard library two ways depending on the active [JDK
mode](../getting-started/jdk-modes.md):

- In **real-JDK mode**, the standard-library *classes* are the real JDK's
  bytecode; CratonVM provides the underlying `native` methods (the ones HotSpot
  implements in C). This is the broadest-coverage path.
- In **synthetic mode**, CratonVM provides Rust implementations of the
  standard-library classes themselves, so no JDK is needed.

Across both, CratonVM registers **thousands of native method implementations**
spanning the Java SE standard library.

## Frequently used packages

| Package | Representative coverage |
|---------|-------------------------|
| `java.lang` | `Object`, `String`, `StringBuilder`/`StringBuffer`, `System`, `Math`, the boxed primitives (`Integer`, `Long`, `Double`, …), `Enum`, `Throwable`, `Thread`, `Runtime`, `Class`, `ClassLoader` |
| `java.util` | `ArrayList`, `LinkedList`, `HashMap`, `LinkedHashMap`, `TreeMap`, `HashSet`, `TreeSet`, `ArrayDeque`, `PriorityQueue`, `Vector`, `Stack`, `Arrays`, `Collections`, `Optional`, `StringJoiner`, `Random`, `UUID`, `Properties`, `Base64` |
| `java.util.stream` | `Stream`, `IntStream`, `LongStream`, `DoubleStream`, `Collectors` (lazy/short-circuiting pipeline by default) |
| `java.util.function` | `Function`, `Consumer`, `Predicate`, `Supplier`, `BiFunction`, `Comparator`, and the rest of the functional interfaces |
| `java.util.concurrent` | `ConcurrentHashMap`, `CopyOnWriteArrayList`, `ReentrantLock`, `CountDownLatch`, `Semaphore`, `CyclicBarrier`, executors |
| `java.util.regex` | `Pattern`, `Matcher` |
| `java.io` | `PrintStream`, `PrintWriter`, `InputStream`/`OutputStream` families, `ByteArrayInputStream`/`ByteArrayOutputStream`, `Scanner`, file streams, `RandomAccessFile` |
| `java.nio` | `ByteBuffer`, `FileChannel`, channels and selectors (see the [Platform Support Matrix](../reference/platform-support.md)) |
| `java.time` | `LocalDate`, `LocalTime`, `LocalDateTime`, `Instant`, `Duration`, `Period` |
| `java.lang.ref` | `WeakReference`, `SoftReference`, `PhantomReference`, `ReferenceQueue` |
| `java.security` / `javax.crypto` | Digests, HMAC, AES/AES-GCM, RSA, DSA, ECDSA/Ed25519, PBKDF2, ML-KEM/ML-DSA (see [Cryptography](../security/cryptography.md)) |

## Networking & I/O

File I/O, memory-mapped files, pipes, watch services, sockets, server sockets,
datagram/UDP and multicast, NIO channels and selectors, and process spawning are
implemented, with platform-specific backends. Exactly which features are
**Full**, **Partial**, or **Stub** on Linux, Windows, and macOS is enumerated in
the [Platform Support Matrix](../reference/platform-support.md).

## Generating an exact coverage catalog

The set of natives CratonVM registers can be extracted directly from the source.
From a checkout, list every registered native (class, method, descriptor):

```bash
rg -N --no-filename -o \
  'r\.register\s*\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"([^"]+)"' -r '$1|$2|$3' \
  native-builtins/src native-collections/src native-io/src native-awt/src \
  | sort -u
```

This catalogs the **formally registered** natives. The total native surface is
larger, because some behavior is satisfied by bytecode and synthetic
implementations that don't go through the registration call site. The repository
ships a generated `docs/JDK_COVERAGE.md` catalog produced this way, and
`docs/synthetic_methods.md` documents the synthetic-implementation surface.

## What's not covered

Some standard-library areas are unimplemented or partial — notably full TLS/JSSE,
JDBC, and on-screen AWT/Swing rendering. See [Known Limitations](limitations.md)
for the list, and use the [missing-natives
audit](../user-guide/debugging.md#finding-missing-standard-library-methods) to
see exactly what a given program needs.
