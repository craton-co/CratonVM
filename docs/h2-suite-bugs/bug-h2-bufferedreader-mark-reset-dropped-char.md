# H2 — `BufferedReader.read` shim breaks `mark()`/`reset()` (dropped first char)

## Status
**FIXED** (worktree `fix/h2-suite-loop`, `native-builtins/src/phases_late.rs`).

## Severity
**HIGH** — corrupts every `RUNSCRIPT`/CSV/`INIT` read and any code using
`BufferedReader.mark()/reset()`. Silent data corruption (wrong SQL/results),
not a clean error.

## Symptom
RUNSCRIPT of a script whose first byte is not a UTF-8 BOM loses its first
character:
```
org.h2.jdbc.JdbcSQLSyntaxErrorException: Syntax error in SQL statement "[*]reate table ..."
   -- "create table" was read as "reate table"
```
CSV: value `"LOWER"` read back as `"OWER"`. Script comment `"-- H2 2.4.249..."`
read as `"- H2 ..."`.

Minimal reproduction (both JIT and `--nojit`, so **not** a JIT bug):
```java
BufferedReader br = new BufferedReader(new InputStreamReader(
    new ByteArrayInputStream("create table".getBytes())));
br.mark(1);
br.read();        // consumes 'c'
br.reset();       // should rewind
br.readLine();    // CratonVM: "reate table"   HotSpot: "create table"
```
Reflection on the real fields after `read(cbuf,0,5)` shows
`nextChar=0, nChars=0` **unchanged** — the read never touched BufferedReader's
buffer.

## HotSpot behavior
PASS — `mark`/`reset` rewind correctly.

## Root cause
`native-builtins/src/phases_late.rs` (`register_phase57_nio_file`, tag RWF86.1)
registers **ungated** native shims for `BufferedReader.read([CII)I` and
`read()I`. For any reader not found in `BR_SIDETABLE` they delegate `read`
**straight to the wrapped `Reader` at field slot 0**, bypassing
`java.io.BufferedReader`'s own buffer (`cb`/`nChars`/`nextChar`) and its
`mark()`/`reset()` bookkeeping.

`BR_SIDETABLE` is **dead**: its only populator, `br_sidetable_register`, is
`#[allow(dead_code)]` and never called — `Files.newBufferedReader` now builds a
*real* `BufferedReader(new StringReader(...))`. So the side-table lookup always
misses and the shim fires for **every** `BufferedReader` in the program.

Because `read` is served by the native (direct delegation) while `mark()` /
`reset()` run the *real* bytecode over the now-unused buffer fields, a
`mark(); read(); reset()` sequence loses whatever the native read consumed from
the underlying reader: `reset()` rewinds `nextChar`, but the underlying reader's
position already advanced and is never rewound. H2's RUNSCRIPT/CSV BOM probe
`reader.mark(1); if (reader.read()!=BOM) reader.reset();`
(`org.h2.command.dml.RunScriptCommand`) therefore drops the first character.

A found native callback is authoritative in CratonVM's dispatch (it returns the
native's result directly; `Ok(None)` does not fall through to bytecode), so the
shim cannot simply "pass through" for real readers — it must not be registered.

## Fix
Gate the two `BufferedReader.read` shims behind
`#[cfg(feature = "synthetic-jdk")]`. In the default real-JDK build every
`BufferedReader` is a genuine JDK instance, so removing the shim lets the real
buffered `read`/`fill`/`mark`/`reset` bytecode run correctly. The synthetic
side-table helpers are marked `#[allow(dead_code)]` for that build. The WildFly
`ProductConfig`/`Properties.load(Reader)` path that motivated RWF86.1 now goes
through `Files.newBufferedReader` → real `BufferedReader`, so it is unaffected.

## Affected test classes (mem config)
TestInit, TestRunscript, TestCsv, and any RUNSCRIPT/CSV/`mark`-`reset` path.

## Repro probes
`MarkBug.java`, `MarkBug2.java` (reflection field dump), `H2Init.java`
(write-correct/read-corrupt disambiguation) in `apps/h2database/h2/`.
