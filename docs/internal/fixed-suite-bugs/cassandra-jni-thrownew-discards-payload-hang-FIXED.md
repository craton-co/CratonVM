# `CassandraAutoConfigurationTests`: JNI `ThrowNew` payload loss and real-mode reader throughput — FIXED

Status: FIXED 2026-07-17

## Original symptom

`module/spring-boot-cassandra`
`org.springframework.boot.cassandra.autoconfigure.CassandraAutoConfigurationTests`
timed out before producing test output. The original trace showed a native
`jnr-posix` provider probe calling JNI `ThrowNew`, then receiving a fabricated
`IllegalStateException` instead of its requested exception type and message.

## Fix

`vm/src/native/jni.rs` now materializes the caller-supplied exception class
and UTF-8 message for `ThrowNew`, preserves the exception object as a native
root across GC, and makes `ExceptionOccurred`, `ExceptionClear`, and native
return handling use that rooted object. The exception factory accepts the exact
JNI class identity, so the active native class loader is retained instead of
falling back to a name-only lookup.

Residual investigation found two independent real-JDK execution faults:

- The registered `DefaultEventLoop.execute` bridge synchronously ran submitted
  Netty work on the caller. Removing that override restores the real Netty
  bytecode scheduling semantics.
- `sun.nio.cs.StreamDecoder.read(char[], int, int)` refilled only the one or
  two characters requested by `Reader.read()`. Typesafe Config's tokenizer
  therefore made a complete native/virtual read cycle for every comment byte.
  The decoder now keeps decoded read-ahead characters in its GC-safe side table
  and drains them before refilling, without adding fields that the real JDK
  object layout does not expose to the collector.

## Validation

Built a dedicated release executable from the isolated worktree:

`target/cassandra-jni-thrownew-final-r4-20260717.exe`.

The exact Spring Boot suite class passed all 28 tests with JDK 25:

| Mode | Result |
|---|---|
| `--nojit` | PASS, 28 tests, 130.656s (`spring-boot-cassandra-jni-thrownew-r7-release-nojit-20260717`) |
| JIT | PASS, 28 tests, 72.271s (`spring-boot-cassandra-jni-thrownew-r8-release-jit-20260717`) |

The focused JNI regression covers class/message preservation and the GC root;
the event-loop registry regression asserts that no native override remains.
