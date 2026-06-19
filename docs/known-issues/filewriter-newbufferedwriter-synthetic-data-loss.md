# `java.io.FileWriter` / `Files.newBufferedWriter` synthetic shims silently drop all written data

**Severity:** High (data-loss, silent) — any app that writes text through `FileWriter` or
`Files.newBufferedWriter` and later reads it back gets an **empty (0-byte) file**, with no error.
**Status:** 🟢 FIXED — both the `java.io.FileWriter` shim and the `Files.newBufferedWriter` synthetic are
gated real-by-default (opt back in with `CRATONVM_SYNTHETIC_FILEWRITER=1` / `CRATONVM_SYNTHETIC_BUFFERED_WRITER=1`).
Verified: all `FileWriter`/`newBufferedWriter` repro cases flip from 0-bytes to correct content,
byte-identical to HotSpot; no regression (`GeneratedTest` ok=2, repro byte/OSW/one-shot cases unchanged).
**Mode:** both (deterministic; JIT-off census + JIT).
**HotSpot (JDK 25):** correct (data persisted).

## Symptom

A `FileWriter` (or `Files.newBufferedWriter`) write+close reports success but leaves the file empty:

```java
File f = File.createTempFile("schema_export", ".sql");
try (FileWriter w = new FileWriter(f)) { w.write("create table x;\n"); }   // no exception
Files.readAllBytes(f.toPath());   // -> []  (0 bytes on CratonVM; 16 bytes on HotSpot)
```

## Repro (deterministic, minimal — no Hibernate)

`apps/hibernate-orm/.cratonvm-suite/jsonrepro/FileEnc.java` (and `FileFlush.java`). CratonVM vs HotSpot:

| Case | API | HotSpot | CratonVM |
|---|---|---|---|
| `Files.newOutputStream` + bytes | raw nio stream | OK | OK |
| `OutputStreamWriter(new FileOutputStream(f))` | char writer, io stream | OK | OK |
| `OutputStreamWriter(new FileOutputStream(f), UTF_8)` | char writer | OK | OK |
| `OutputStreamWriter(Files.newOutputStream(p), UTF_8)` | char writer, nio stream | OK | OK |
| **`new FileWriter(f)` + flush + close** | **FileWriter** | OK | **0 bytes** |
| **`new BufferedWriter(new FileWriter(f))`** | **FileWriter** | OK | **0 bytes** |
| **`Files.newBufferedWriter(p)` (+ flush)** | **nio convenience** | OK | **0 bytes** |
| `BufferedWriter(OutputStreamWriter(FileOutputStream))` | manual | OK | OK |
| `FileOutputStream` + flush / `Files.write` one-shot | bytes | OK | OK |

Only the two JDK *convenience* constructs fail: **`FileWriter`** and **`Files.newBufferedWriter`**.
`new OutputStreamWriter(new FileOutputStream(f))` — which is exactly what `FileWriter` *is* — works.

## Root cause

`native-io/src/lib.rs` (`register_io_natives`, ~line 4075) registers a synthetic `java.io.FileWriter`
that treats the writer as a **byte-level `FileOutputStream` with the OS fd in field 0**, intercepting its
`<init>` / `write(int|byte[]|String)` / `flush` / `close`. But the JDK's `FileWriter` is a **character**
writer:

```
FileWriter extends OutputStreamWriter
Writer.write(String) -> Writer.write(String,int,int) -> this.write(char[],0,len)
                     -> OutputStreamWriter.write(char[]) -> StreamEncoder.write(...)  [buffers bytes]
close() -> StreamEncoder.close() -> implFlushBuffer -> out.write(encodedBytes)
```

The shim intercepts the *constructor* (stashing an fd) but **does not intercept the char path
`write([CII)V`**, so the actual characters flow through the real `OutputStreamWriter`/`StreamEncoder`
bytecode — whose downstream `out` was never wired up because the constructor that would have built it was
replaced by the fd-shim. The encoded bytes are buffered into a `StreamEncoder` that goes nowhere, and the
intercepted `close`/`flush` only operate on the (empty) field-0 fd. Net: **every character is silently
discarded.** The shim is split-brained — a byte fd on one side, the real char encoder on the other.

This is the **write-side twin of the `InputStreamReader` byte-shim** that was already removed in favor of
real bytecode (see the "RDR-MIGRATION 2026-06-01" note directly below the FileWriter block, lib.rs ~4175).
`Files.newBufferedWriter` has its own separate synthetic (`native-builtins/src/phases_late.rs` ~6132:
returns a synthetic fd-backed `BufferedWriter`), which fails the same way.

Proof the underlying machinery is fine: `OutputStreamWriter` over `FileOutputStream` **and** over the nio
`Files.newOutputStream` both round-trip correctly on CratonVM (table above), as does raw `FileOutputStream`
and `Files.write`. So removing the shim makes `FileWriter` run its real bytecode
(`super(new FileOutputStream(file))`) over already-working components.

## Fix

Mirror the `real_raf_enabled()` idiom (lib.rs ~8603): gate the `java.io.FileWriter` shim **real-by-default**,
opt back into the broken byte-shim only with `CRATONVM_SYNTHETIC_FILEWRITER=1`. With the shim off, real
`FileWriter` bytecode runs `OutputStreamWriter(new FileOutputStream(file))` — proven to work — and data
persists.

`Files.newBufferedWriter` (`native-builtins/src/phases_late.rs`, `register_phase57_nio_file`) had its own
**separate** synthetic (`open_buffered_writer` → synthetic fd-backed `BufferedWriter`). Gated the same way
(real-by-default): the real `Files.newBufferedWriter` is
`BufferedWriter(OutputStreamWriter(Files.newOutputStream(p), encoder))`, all of whose pieces work on
CratonVM, and the resulting real `BufferedWriter` flows correctly through the already-real-aware
`bw_delegate_out` BufferedWriter natives (which remain registered for picocli/JUnit-console output). Proven
safe by replicating the exact real construction (`jsonrepro/BwReal.java`) before flipping the gate.

## NOT this bug (separate findings from the same census cluster)

- **"Unable to open specified script target file for writing : C:\…\Temp\tmp.XXXX"** (GeneratedTest,
  SelectGeneratorTest, OneToOneJoinTableUniquenessTest) — these all **PASS solo**; the failures only appear
  under the 10-shard parallel census (concurrent `%TEMP%` contention). **Environmental, not a CV bug.**
- **H2 `IOException: Системе не удаётся найти указанный путь` / `compiler message file broken`**
  (SessionDelegatorBaseImplTest, StoredProcedureResultSetMappingTest) — H2's in-database Java-compiler
  (`javac` message `ResourceBundle`) path; a **different** issue, not FileWriter.
