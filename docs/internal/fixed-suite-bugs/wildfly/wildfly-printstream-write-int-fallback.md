# WildFly: Surefire event channel trips `PrintStream.write(int)` on synthetic stream

## Status

✅ FIXED — verified 2026-07-06, no longer reproduces on `dev`.

Originally found 2026-07-05 while rerunning WildFly non-passed classes on Azure.

## Symptom

`HostExcludesTestCase` completes without test execution and Surefire reports a
corrupted fork channel. The forked VM dumps an exception while encoding the
`testSetCompleted` event:

```text
java.lang.NullPointerException: Cannot invoke "java.io.OutputStream.write(int)"
    at java.io.PrintStream.write(PrintStream.java:506)
    at org.apache.maven.surefire.api.util.internal.Channels$4.writeImpl(Channels.java:199)
    at org.apache.maven.surefire.booter.spi.EventChannelEncoder.write(EventChannelEncoder.java:288)
    at org.apache.maven.surefire.booter.spi.EventChannelEncoder.testSetCompleted(EventChannelEncoder.java:126)
```

## Evidence (original)

- Host: Azure `victor@20.84.156.31`
- Worktree: `/data/wt/wt-wildfly-nonpassed-20260705-035722`
- Run: `apps/wildfly-suite-runner/out/azure-nonpassed-jiton-005-jit-real-failed-20260705-045244`
- Class log: `logs/00004-org.jboss.as.test.integration.domain.HostExcludesTestCase.log`

## Root cause

The real-JDK fallback registration in `../../../../native-builtins/src/lib.rs` covered
`PrintStream.write(byte[], int, int)` plus string writer paths, but not
`PrintStream.write(int)`. Surefire's channel writes single bytes to the forked
process's native stream, so CratonVM dispatched to real JDK bytecode. The
synthetic `System.out`/`System.err` object has no populated
`FilterOutputStream.out`, and the bytecode dereferenced that null field.

## Fix

`java/io/PrintStream.write(I)V` is now registered in
`register_printstream_fallback_natives` (`../../../../native-builtins/src/lib.rs`,
`native_printstream_write_int`), routing the low byte through the same
fd-aware path (`with_stdio_print_lock` / `surefire_forwarding_write` /
`route_write_through_out` / `stream_fd`) used by the byte-array write native.
This landed in `a3728860` ("Fix WildFly non-passed suite blockers",
2026-07-05), bundled with the related Surefire event-channel fixes tracked in
`docs/internal/wildfly-suite-bugs/bug-13-surefire-event-channel-buffered-output-interleaving.md`.
The doc wasn't updated at the time, so it stayed listed as open despite the
underlying gap already being closed.

## Verification (2026-07-06)

Built a disposable worktree off current `dev` (`12338461`) on the Azure host
and ran a minimal probe reproducing the exact call shape from the crash
(`PrintStream.write(int)` on the synthetic system streams, bypassing
`print`/`println`):

```java
public class WriteIntProbe {
    public static void main(String[] args) throws Exception {
        System.out.write(65);
        System.out.write(66);
        System.out.flush();
        System.err.write(67);
        System.err.flush();
        System.out.println();
        System.out.println("write(int) OK");
    }
}
```

Run under `cratonvm --java-home <real JDK25>` (real-JDK mode, the mode the
original bug occurred in):

```text
ABC
write(int) OK
[cratonvm] main-vm run() returned Ok — VM main exiting normally
```

**Negative control:** removed just the `write(I)V` registration from the same
build and reran the identical probe — it reproduced the *exact* original
exception, confirming this registration (and not some unrelated change) is
what fixes the bug:

```text
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "java.io.OutputStream.write(int)"
	at WriteIntProbe.main(WriteIntProbe.java:6)
	at java/io/PrintStream.write(PrintStream.java:506)
```

Restored the file and removed the disposable worktree afterward; no code
change was needed since the fix was already on `dev`.

Moved out of `../../../known-issues` per the "only unfixed bugs" convention.
