# WildFly: Surefire event channel trips `PrintStream.write(int)` on synthetic stream

## Status

Open as of 2026-07-05 while rerunning WildFly non-passed classes on Azure.

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

## Evidence

- Host: Azure `victor@20.84.156.31`
- Worktree: `/data/wt/wt-wildfly-nonpassed-20260705-035722`
- Run: `apps/wildfly-suite-runner/out/azure-nonpassed-jiton-005-jit-real-failed-20260705-045244`
- Class log: `logs/00004-org.jboss.as.test.integration.domain.HostExcludesTestCase.log`

## Root-cause hypothesis

The real-JDK fallback registration in `native-builtins/src/lib.rs` covers
`PrintStream.write(byte[], int, int)` plus string writer paths, but does not cover
`PrintStream.write(int)`. Surefire's channel writes single bytes to the forked
process native stream, so CratonVM dispatches to JDK bytecode. The synthetic
`System.out`/`System.err` object has no populated `FilterOutputStream.out`, and
the bytecode dereferences that null field.

## Expected fix

Register `java/io/PrintStream.write(I)V` in the real-JDK fallback block and route
the low byte through the same fd-aware path used by the byte-array write native.
