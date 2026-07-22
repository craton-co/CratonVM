# Misc spring-core cluster вЂ” two independent issues

> **UPDATE 2026-07-01:** Issue A is fixed on current `dev`. `native_chm_remove`
> already rejected null keys, and this pass added the same JDK-parity null-key
> guard to `ConcurrentHashMap.get`, `containsKey`, `getOrDefault`, and
> `remove(key,value)`, with registry-level coverage in
> `concurrent_hashmap_null_key_methods_throw_npe`. Issue B remains a handoff
> pending empirical OutputStream-vs-Writer capture.

This cluster contains TWO distinct root causes. They are unrelated; assess/fix separately.

---

## Issue A вЂ” `SimpleAliasRegistryTests.removeNullAlias` (HIGH confidence, FIX-ready)

> **STATUS: RESOLVED on dev.** The historical analysis below is retained for
> provenance. `ConcurrentHashMap.remove(null)` now throws `NullPointerException`;
> this pass also covered the sibling null-key read/removal methods listed below.

### Symptom
`removeNullAlias` asserts `registry.removeAlias(null)` throws `NullPointerException`. Under CratonVM no NPE is thrown; instead an `IllegalStateException("No alias 'null' registered")` is thrown, so AssertJ's `assertThatNullPointerException()` fails (empty-message AssertionError).

### Affected test
`org.springframework.core.SimpleAliasRegistryTests.removeNullAlias` (SimpleAliasRegistryTests.java:93-95).

### Root cause (file:line)
Spring's `SimpleAliasRegistry.removeAlias` (spring-core `.../core/SimpleAliasRegistry.java:114-123`):
```java
String name = this.aliasMap.remove(alias);   // aliasMap is a ConcurrentHashMap
this.aliasNames.remove(alias);
if (name == null) throw new IllegalStateException("No alias '" + alias + "' registered");
```
`aliasMap` is `new ConcurrentHashMap<>(16)` (line 50). On HotSpot, `ConcurrentHashMap.remove(null)` computes `spread(key.hashCode())` and throws NPE on the null key, which propagates straight out of `removeAlias` вЂ” that is exactly the behavior the test expects.

CratonVM diverges in `native-collections/src/lib.rs`:
- `native_chm_remove` (~line 25528): reads the key via `args.get(1).copied().unwrap_or(Value::Object(None))`, calls `chm_key_hash(null)` which returns `Ok(0)` (`chm_key_hash`, ~line 24806: the non-`Object(Some)` arm yields `Ok(0)`), then delegates to `native_map_remove`, which treats a null key as `is_null_key` and simply returns `Value::Object(None)` (not found). **No NPE is ever raised.**
- By contrast `native_chm_put` (line 25503, guard at 25510-25516) and `native_chm_put_if_absent` (line 25545, guard at 25552-25558) explicitly `return Err(RuntimeError::NullPointerException ...)` on a null key/value.

Because `remove(null)` returns null instead of throwing, Spring proceeds to `name == null` and throws ISE. Test fails.

The same missing null-key guard also affects `native_chm_get` (25431), `native_chm_contains_key` (25446), `native_chm_get_or_default` (25468), and `native_chm_remove_kv` (25897). The JDK throws NPE for null keys on `get`/`containsKey`/`remove`/`remove(k,v)` as well, so these are latent divergences worth fixing in the same pass.

### Reproduction sketch
```java
import java.util.concurrent.ConcurrentHashMap;
public class ChmRemoveNull {
  public static void main(String[] a){
    ConcurrentHashMap<String,String> m = new ConcurrentHashMap<>();
    try { m.remove(null); System.out.println("NO NPE (bug)"); }
    catch (NullPointerException e){ System.out.println("NPE (correct)"); }
  }
}
```
Run: `cratonvm --java-home <jdk25> ChmRemoveNull` вЂ” CratonVM prints `NO NPE (bug)`; HotSpot throws NPE.

### Recommendation
FIX. Add a null-key guard at the top of `native_chm_remove` mirroring `native_chm_put`:
```rust
let key = args.get(1).copied().unwrap_or(Value::Object(None));
if matches!(key, Value::Object(None)) {
    return Err(RuntimeError::NullPointerException { message: None }.into());
}
```
For full JDK parity also add the guard to `native_chm_get`, `native_chm_contains_key`, `native_chm_get_or_default`, and `native_chm_remove_kv`. Low-risk, localized.

### Severity / Confidence
Severity medium (correctness divergence on a documented JDK contract; could mask real null-key bugs in app code). Confidence HIGH вЂ” the code path is fully traced and the contrast with put/putIfAbsent is unambiguous.

### Open questions
None for the alias test itself.

---

## Issue B вЂ” `SortedPropertiesTests.sortsProperties*UsingOutputStream` (LOW-MED confidence, HANDOFF)

### Symptom
Both `OutputStream`-based `store()` tests fail with an empty-message AssertionError (most likely `assertThat(lines).hasSize(7)` / `hasSize(5)`, or `assertPropsAreSorted`). The two `Writer`-based variants (`sortsPropertiesUsingWriter`, `sortsPropertiesAndOmitsCommentsUsingWriter`) are reported passing.

### Affected tests
- `SortedPropertiesTests.sortsPropertiesUsingOutputStream` (line 71, expects 7 lines)
- `SortedPropertiesTests.sortsPropertiesAndOmitsCommentsUsingOutputStream` (line 100, expects 5 lines)

### What the test does
`SortedProperties.store(OutputStream, comment)` (spring-core `.../core/SortedProperties.java:90-99`):
1. inner `super.store(baos, comment-or-null)` вЂ” the real JDK `Properties.store(OutputStream, String)` over `new BufferedWriter(new OutputStreamWriter(out, ISO_8859_1))` (`store0` with `escUnicode=true`);
2. `baos.toString(StandardCharsets.ISO_8859_1)`;
3. split on `EOL = System.lineSeparator()` (`\r\n` on this Windows box), drop `#` lines when `omitComments`, re-emit.

The Writer variant differs only by wrapping the user `StringWriter` directly (`store0` with `escUnicode=false`) and using `stringWriter.toString()` instead of `baos.toString(Charset)`.

### Root cause вЂ” NOT definitively isolated by static analysis
I traced every component exclusive to the failing OutputStream path and each appears correct for the ASCII content used by the test:
- StreamEncoder shim `native-io/src/stream_encoder.rs` (`native_se_write_string` slicing, `forOutputStreamWriter`) and the ISO-8859-1 encoder (`native-api/src/charset.rs:130` `encode_latin1`, lossy fallback 203-206) are byte-identity for `\r`/`\n`/ASCII.
- The `BufferedWriter` `write`/`newLine`/`flush`/`close` natives in `native-builtins/src/phases_late.rs:6554-6680` shadow EVERY `BufferedWriter` even in real-JDK mode (`bw_delegate_out`, line 3864, forwards to the wrapped `out`; `newLine` reads the `line.separator` property = `\r\n`). **But these are also on the Writer path**, so they cannot explain an OutputStream-only failure.
- `baos.toString(Charset)` falls through to real JDK bytecode (`new String(buf,0,count,ISO_8859_1)`): the BAOS allow-list in `vm/src/vm/vm_exec.rs:10644-10648` is descriptor-keyed via the `.is_some()` conjunction at 10811-10816, and only `toString()` / `toString(String)` have natives вЂ” so the `Charset` overload is NOT overridden and decodes ISO-8859-1 correctly against the synthetic BAOS slots 0/1 (`buf`/`count`, `native-io/src/lib.rs:2885-2886`, populated by the real `<init>` bytecode + `native_baos_write_bytes`).
- The sorted iteration order is driven by Spring's own `entrySet()`/`keys()` overrides (which call `super.entrySet()` в†’ CratonVM `native_properties_entry_set`, `native-builtins/src/properties_sidetable.rs:1668`); this is shared with the Writer path.
- `new Date().toString()` is single-line ASCII (`"EEE MMM dd HH:mm:ss zzz yyyy"`, deprecated_io_util.rs:411), so the timestamp comment is one `#` line вЂ” also shared.

Every candidate that could change the line count (line separator, Date format, sort order, split behavior) is SHARED with the passing Writer variant, so a purely-OutputStream divergence could not be pinned statically. The failure likely lies in a runtime interaction not visible to static reading (e.g. how `store0`'s `escUnicode=true` path or the OSW/StreamEncoder buffering composes at runtime, or a subtle byte the inner store emits). Empirical repro is required.

### RELATED latent bug (real, but DORMANT for these tests)
`native-io/src/lib.rs:3147-3149` `native_baos_to_string_charset` **ignores the charset-name argument** and delegates to `native_baos_to_string`, which decodes via `String::from_utf8_lossy` (line 3142). It is registered unconditionally (real-JDK too) at `lib.rs:4591-4596` for `ByteArrayOutputStream.toString(String)`. So `baos.toString("ISO-8859-1")` (or any charset NAME) always decodes UTF-8, corrupting any high bytes. This is a genuine VM bug, but it does NOT affect SortedProperties, which uses the `toString(Charset)` overload (correctly handled). Worth fixing independently (decode using the requested charset via the charset engine), and worth checking whether any other failing test uses the String-name overload.

### Reproduction sketch
```java
import java.util.Properties;
import java.io.*;
import java.nio.charset.StandardCharsets;
public class PropStore {
  public static void main(String[] a) throws Exception {
    Properties p = new Properties();
    p.setProperty("b","2"); p.setProperty("a","1");
    ByteArrayOutputStream baos = new ByteArrayOutputStream();
    p.store(baos, "c");
    String s = baos.toString(StandardCharsets.ISO_8859_1);
    System.out.println("OUTSTREAM_LINES=" + s.trim().split(System.lineSeparator()).length);
    System.out.print(s);
    StringWriter w = new StringWriter();
    p.store(w, "c");
    System.out.println("WRITER_LINES=" + w.toString().trim().split(System.lineSeparator()).length);
  }
}
```
Run: `cratonvm --java-home <jdk25> PropStore` and diff both the line counts and the exact bytes against HotSpot; the goal is to confirm the OutputStream output diverges (count or content) while the Writer output matches.

### Suspected subsystem
native-io `OutputStreamWriter`/`StreamEncoder` + `ByteArrayOutputStream`; native-builtins shadowed `BufferedWriter` natives (phases_late.rs).

### Severity / Confidence
Severity medium. Confidence LOW-MEDIUM вЂ” concrete, related code defect identified (BAOS `toString(String)`), but the exact cause of these two failures was not isolable statically.

### Recommendation
HANDOFF for empirical confirmation: run the repro above, capture the raw bytes/line counts for OutputStream vs Writer, and binary-search the difference (timestamp line vs property lines vs separator). Independently, fix the latent `native_baos_to_string_charset` charset-ignore.

### Open questions
- Do both Writer variants genuinely pass on the box (which would confirm the divergence is strictly in the OSW/StreamEncoder/baos chain)?
- Does the OutputStream output differ in line COUNT or only in content (which assertion fails вЂ” `hasSize` vs `assertPropsAreSorted` vs the `startsWith("#")` timestamp check)?
- Is the inner-store byte stream byte-identical to HotSpot (confirms StreamEncoder correctness)?
