# The `ZipFile`/`JarFile` redefine immunity could not see a mock from an archive

**Status: FIXED 2026-09-10.** Closes
`loader/spring-boot-loader … UrlJarFilesTests` (2 of 11 tests), a CratonVM-only
Spring Boot failure whose HotSpot 25 baseline is `PASS 0/11`.

| arm | before | after |
|---|---|---|
| `org.springframework.boot.loader.net.protocol.jar.UrlJarFilesTests` | 11 tests, **2 failed** | 11 tests, **0 failed** |
| `…jar.JarUrlConnectionTests` (the class the immunity was added FOR) | 47 tests, 0 failed | 47 tests, **0 failed** |
| the whole `loader/*` subtree, 83 classes | 82 clean, 1 failing | **83 clean** |

## Symptom

Two tests fail, and neither names its own defect:

```text
UrlJarFilesTests:getOrCreateWhenUsingCachingReturnsCachedWhenAvailable()
  => org.mockito.exceptions.misusing.UnfinishedVerificationException:
     Missing method call for verify(mock) here:
     -> at …UrlJarFilesTests.closeIfNotCachedWhenNotCachedClosesJarFile(UrlJarFilesTests.java:133)
     …
     at …UrlJarFilesTests.<init>(UrlJarFilesTests.java:51)
```

The exception surfaces in the CONSTRUCTOR of a LATER test, and names an
EARLIER one. Line 133 is

```java
JarFile jarFile = mock(JarFile.class);
this.jarFiles.closeIfNotCached(this.url, jarFile);
then(jarFile).should().close();          // <- starts a verification
```

`then(jarFile).should()` opens a verification that the following `.close()`
must consume. On CratonVM the `.close()` was never recorded, so the
verification stayed open and Mockito reported it at the next `mock()` in the
class — two tests later. The two failing test names are therefore innocent
bystanders; the defect is in `closeIfNotCachedWhen*`.

## Root cause

Reduced to a probe with no Spring in it at all:

```java
JarFile jf = mock(JarFile.class);
when(jf.getName()).thenReturn("STUBBED");   // MissingMethodInvocationException
jf.getName();                               // null
mockingDetails(jf).getInvocations().size(); // 0
```

Nothing on a mocked `JarFile` is intercepted — not stubbing, not verification,
not recording. A `java.io.File` mock in the same process is fine (`invocations=1`).

`redefine_immune_zip_file_native` is why, and it is doing exactly what it was
written to do. `ZipFile`/`JarFile` archives live in a Rust handle table
(`native-io/src/zip_real_jar.rs`), not in the JDK's `res`/`zsrc` field graph, so
when Mockito's inline mock maker retransforms the class — it retransforms the
whole chain; measured here as `retransformClasses0 called with 6 classes:
java/util/jar/JarFile, java/util/zip/ZipFile, java/util/zip/ZipConstants,
java/io/Closeable, java/lang/AutoCloseable, java/lang/Object` — the
suppress-native-shadow-on-redefine rule must NOT fire, or the next
`getInputStream` on any real archive runs a JDK body against an instance that
has no `res`. That is `JarUrlConnectionTests`' NPE, and the immunity fixed it.

The immunity is keyed on (class, method). It cannot be, because for these two
classes the same (class, method) has two opposite right answers:

* a **real archive** has a handle in `jar_table()` and no `res` — it must stay
  on the native;
* a Mockito **inline mock** of the same class is Objenesis-allocated, so no
  constructor ran, so it has no handle EITHER — the native can only answer
  `null`/`0` for it, silently, and the woven advice is the only body that can
  answer at all.

Under the old rule the mock got the native, and every call on it was a silent
no-op that recorded nothing.

## Fix

Ask the immunity one more question — *is this receiver an archive we actually
opened?* — and stand it down when the answer is no.

1. `native-io/src/zip_real_jar.rs` publishes `identity_is_known_archive(i32)`,
   backed by an APPEND-ONLY set of the identity hashes `open_and_register` has
   handed a handle to. Append-only on purpose: `identity_handle_table` drops its
   entry at `close`, and a closed `JarFile` is still a real archive whose
   `close`/`getName` must not reach `ZipFile`'s real body.
2. `zip_immunity_waived_for_receiver` composes that with the existing
   class/method arm, and `redefine_immune_forced_native_for_receiver` is the
   drop-in the dispatch gates call. `<init>` is never waived — a real archive's
   handle is registered BY the constructor, so at its entry no receiver has one.
3. Six dispatch gates were converted to pass the receiver:
   `invoke.rs`'s ancestor walk, its shadow-drop gate and its force-native gate;
   `vm_exec.rs`'s `invoke_or_native` and `invoke_on_class_shared_inner`; and
   both `intercept_force_registered_native{,_cached}`.
4. `redefine_immune_layout_native` — the invoke-cache aggregator — **drops the
   zip arm**. This is the one arm whose two aggregators must differ. Every other
   member is decided by the class, so a receiver-blind cache entry is a correct
   one; this one is not, and a per-call-site cache cannot express "native for
   that instance, bytecode for this one". With the arm gone the cache refuses to
   decide and dispatch falls through to the slow path, which has the receiver.

## How the gate was found, and the two false finishes

Worth recording, because five of the six gates are decoys for this symptom.

The first patch converted the two obvious gates in `invoke.rs`. `mock(JarFile
.class).close()` still recorded nothing. The second added `vm_exec.rs`'s two.
`CRATONVM_DBG=native-shadow` then read

```text
[native-shadow] java/util/jar/JarFile.close()V  dropped=true … immune=false
```

— the waiver was firing, the native was being dropped, and the behaviour had
not changed at all. The third added the two force-native interceptors. Still
nothing.

What settled it was a kill switch rather than another guess:
`CRATONVM_DBG_ZIPIMMUNE=off` forces `redefine_immune_zip_file_native` to
`false` and prints one line per (class, method) it is consulted for. With it
the mock stubbed and recorded perfectly, which proves the immunity IS the gate;
and the trace showed the `java/util/zip/ZipFile` consultations arriving with no
waiver line beside them, which named the caller that was still blind. That was
`try_stackless_invoke`'s force-native gate — a SECOND gate in a function whose
other two had already been converted.

The lever is left in the tree. `=1` traces, `=off` disables; both are one
`OnceLock` read on a path that only runs after a redefinition.

## Validation

* `UrlJarFilesTests` 11/11, `JarUrlConnectionTests` 47/47.
* The whole `loader/*` subtree, 83 classes, one process each: 83 clean after,
  against 82 clean + `UrlJarFilesTests` before. Same binary lineage, same
  harness, same host.
* `leaves_the_zip_arm_to_the_receiver_aware_slow_path` replaces
  `zip_and_jar_layout_natives_survive_a_redefinition` and asserts the new
  contract in both directions — immune on the slow-path aggregator, NOT immune
  on the invoke-cache one — so the next person to "restore symmetry" between
  the two aggregators fails a test that explains itself.
* The source-witness gate that forbids naming an arm outside an aggregator has
  `zip_immunity_waived_for_receiver` added to its exempt list, with the reason:
  composing arms is that function's job, and no dispatch site names an arm.

## Related

* `spring-boot-loader-zipfile-close-invokespecial-native-bypass-npe-FIXED.md`
  and `loader-jar-nested-url-connection-npe-pair-FIXED-20260905.md` — the same
  subsystem, the other direction.
* `threadlocal-retransform-drops-native-shadow-FIXED.md` — the immunity
  mechanism itself, and the reason it exists.
* `real_http_url_connection_native` in `native_override.rs` reached the same
  idea independently and states it well: an Objenesis-constructed mock never ran
  a constructor, so its field 0 stays null. This asks the archive tables the
  same question, which is exact rather than a proxy for it.
