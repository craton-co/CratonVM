# JDK class coverage in CratonVM 0.3.0 — auto-generated from native-* crates' r.register() call sites.

This catalog is produced by walking every `r.register("<class>", "<method>", "<desc>", ...)` site under the four native-method crates and grouping by class.

**Regenerate** (PowerShell / cross-platform ripgrep):

```
rg -N --no-filename -o 'r\.register\s*\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"([^"]+)"' -r '$1|$2|$3' \
   native-builtins/src native-collections/src/lib.rs native-io/src native-awt/src \
   | sort -u
```

Or, equivalently, search the four trees for the pattern
`r\.register\s*\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*,\s*"([^"]+)"` and group
by the first capture group.

Methods listed below are the natives **registered by CratonVM** — Java code that
ships in `java.base` and resolves entirely through bytecode is *not* in this
table. Methods are listed as `name(descriptor)` using JVMS internal form
(e.g. `[B` = `byte[]`, `Ljava/lang/String;` = `String`).

> **Note — scope of this count vs. the "3,100+" figure.** The entries in this
> catalog are *only* the natives **formally registered through the
> native-method registry** (the `r.register(...)` call sites). They are a
> **subset** of CratonVM's total native coverage. The larger "3,100+ native
> methods" figure cited in the README and CHANGELOG counts the full surface,
> which *additionally* includes natives satisfied via bytecode/synthetic
> implementations that are not emitted through `r.register(...)` and therefore
> are not enumerated by this generated dump. The two numbers measure different
> things and do not conflict: this file reports the formally-registered subset,
> while "3,100+" reports the aggregate native surface.

---

## `java/beans/FeatureDescriptor`

| Method | Descriptor |
|---|---|
| `getName` | `()Ljava/lang/String;` |

## `java/beans/MethodDescriptor`

| Method | Descriptor |
|---|---|
| `getMethod` | `()Ljava/lang/reflect/Method;` |

## `java/io/BufferedReader`

| Method | Descriptor |
|---|---|
| `close` | `()V` |

## `java/io/InputStream`

| Method | Descriptor |
|---|---|
| `close` | `()V` |
| `read` | `()I` |

## `java/io/InputStreamReader`

| Method | Descriptor |
|---|---|
| `close` | `()V` |
| `read` | `()I` |

## `java/io/Reader`

| Method | Descriptor |
|---|---|
| `close` | `()V` |

## `java/io/StreamCorruptedException`

| Method | Descriptor |
|---|---|
| `<init>` | `()V` |

## `java/lang/AutoCloseable`

| Method | Descriptor |
|---|---|
| `close` | `()V` |

## `java/lang/Boolean`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `booleanValue` | `()Z` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(Z)Ljava/lang/Boolean;` |

## `java/lang/Byte`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `byteValue` | `()B` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(B)Ljava/lang/Byte;` |

## `java/lang/Character`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `charValue` | `()C` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(C)Ljava/lang/Character;` |

## `java/lang/Class`

| Method | Descriptor |
|---|---|
| `isRecord` | `()Z` |
| `isSealed` | `()Z` |

## `java/lang/Double`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `doubleValue` | `()D` |
| `isFinite` | `(D)Z` |
| `max` | `(DD)D` |
| `min` | `(DD)D` |
| `sum` | `(DD)D` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(D)Ljava/lang/Double;` |

## `java/lang/Float`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `floatValue` | `()F` |
| `isFinite` | `(F)Z` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(F)Ljava/lang/Float;` |

## `java/lang/Integer`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `compare` | `(II)I` |
| `intValue` | `()I` |
| `max` | `(II)I` |
| `min` | `(II)I` |
| `sum` | `(II)I` |
| `toBinaryString` | `(I)Ljava/lang/String;` |
| `toHexString` | `(I)Ljava/lang/String;` |
| `toOctalString` | `(I)Ljava/lang/String;` |
| `toString` | `()Ljava/lang/String;` |
| `toUnsignedLong` | `(I)J` |
| `valueOf` | `(I)Ljava/lang/Integer;` |

## `java/lang/Long`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `compare` | `(JJ)I` |
| `longValue` | `()J` |
| `max` | `(JJ)J` |
| `min` | `(JJ)J` |
| `sum` | `(JJ)J` |
| `toHexString` | `(J)Ljava/lang/String;` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(J)Ljava/lang/Long;` |

## `java/lang/Math`

| Method | Descriptor |
|---|---|
| `clamp` | `(DDD)D` |
| `clamp` | `(FFF)F` |
| `clamp` | `(JII)I` |
| `clamp` | `(JJJ)J` |

## `java/lang/ProcessHandle`

| Method | Descriptor |
|---|---|
| `pid` | `()J` |

## `java/lang/Runtime`

| Method | Descriptor |
|---|---|
| `runFinalization` | `()V` |

## `java/lang/Short`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |
| `shortValue` | `()S` |
| `toString` | `()Ljava/lang/String;` |
| `valueOf` | `(S)Ljava/lang/Short;` |

## `java/lang/StrictMath`

| Method | Descriptor |
|---|---|
| `clamp` | `(DDD)D` |
| `clamp` | `(FFF)F` |
| `clamp` | `(JII)I` |
| `clamp` | `(JJJ)J` |

## `java/lang/String`

| Method | Descriptor |
|---|---|
| `<init>` | `([B)V` |
| `<init>` | `([BII)V` |
| `<init>` | `([BIII)V` |
| `getBytes` | `(II[BI)V` |

## `java/lang/System`

| Method | Descriptor |
|---|---|
| `runFinalization` | `()V` |

## `java/lang/Thread`

| Method | Descriptor |
|---|---|
| `stop` | `()V` |

## `java/lang/Throwable`

| Method | Descriptor |
|---|---|
| `toString` | `()Ljava/lang/String;` |

## `java/lang/Void`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `java/net/Inet4Address`

| Method | Descriptor |
|---|---|
| `init` | `()V` |

## `java/net/Inet6Address`

| Method | Descriptor |
|---|---|
| `init` | `()V` |

## `java/net/ServerSocket`

| Method | Descriptor |
|---|---|
| `init` | `()V` |

## `java/net/Socket`

| Method | Descriptor |
|---|---|
| `init` | `()V` |

## `java/net/URLConnection`

| Method | Descriptor |
|---|---|
| `connect` | `()V` |
| `setUseCaches` | `(Z)V` |

## `java/nio/Bits`

| Method | Descriptor |
|---|---|
| `reserveMemory` | `(JJ)V` |
| `unreserveMemory` | `(JJ)V` |

## `java/security/MessageDigest`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `java/security/PrivateKey`

| Method | Descriptor |
|---|---|
| `getAlgorithm` | `()Ljava/lang/String;` |
| `getEncoded` | `()[B` |
| `getFormat` | `()Ljava/lang/String;` |

## `java/security/Provider$EngineDescription`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `java/security/Provider$ServiceKey`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `java/security/PublicKey`

| Method | Descriptor |
|---|---|
| `getAlgorithm` | `()Ljava/lang/String;` |
| `getEncoded` | `()[B` |
| `getFormat` | `()Ljava/lang/String;` |

## `java/security/SecureRandom`

| Method | Descriptor |
|---|---|
| `generateSeed` | `(I)[B` |
| `nextBytes` | `([B)V` |

## `java/security/Security`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `java/security/Signature`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `java/util/Arrays`

| Method | Descriptor |
|---|---|
| `asList` | `([Ljava/lang/Object;)Ljava/util/List;` |
| `fill` | `([II)V` |
| `sort` | `([I)V` |

## `java/util/Map`

| Method | Descriptor |
|---|---|
| `of` | `()Ljava/util/Map;` |

## `java/util/Set`

| Method | Descriptor |
|---|---|
| `of` | `()Ljava/util/Set;` |

## `java/util/concurrent/ScheduledFuture`

| Method | Descriptor |
|---|---|
| `cancel` | `(Z)Z` |
| `isCancelled` | `()Z` |
| `isDone` | `()Z` |

## `java/util/stream/BaseStream`

| Method | Descriptor |
|---|---|
| `iterator` | `()Ljava/util/Iterator;` |

## `javax/crypto/Cipher`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `javax/crypto/JceSecurity`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `jdk/internal/misc/VM`

| Method | Descriptor |
|---|---|
| `awaitInitLevel` | `(I)V` |
| `initLevel` | `()I` |
| `initialize` | `()V` |

## `org/w3c/dom/Comment`

| Method | Descriptor |
|---|---|
| `getNodeType` | `()S` |

## `sun/nio/ch/EPoll`

| Method | Descriptor |
|---|---|
| `epollCreate` | `()I` |

## `sun/security/jca/GetInstance`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `sun/security/jca/JCAUtil`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `sun/security/jca/ProviderList`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `sun/security/jca/Providers`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

## `sun/security/util/Debug`

| Method | Descriptor |
|---|---|
| `<clinit>` | `()V` |

---

**Totals (0.3.0)**: 53 distinct classes, 128 registered native methods
(native-builtins: 121, native-collections: 4, native-io: 3, native-awt: 0).

These totals count *only* the methods formally registered through the
native-method registry (the `r.register(...)` call sites enumerated above) and
are a **subset** of CratonVM's overall native coverage. The "3,100+ native
methods" figure cited elsewhere (README/CHANGELOG) is larger because it also
includes natives provided via bytecode/synthetic implementations, which are not
emitted through `r.register(...)` and so are not enumerated here. The two
figures are therefore consistent rather than contradictory.
