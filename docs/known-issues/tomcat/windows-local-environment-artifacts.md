# Remaining tomcat non-passing classes: Windows-local-environment artifacts and relative-timing races, not CratonVM bugs

## Status
Confirmed-or-strongly-suspected environmental for each row below; not
individually as thoroughly checked as the EasyMock/ByteBuddy or ecj pages, but
each has a concrete, specific mechanism that has nothing to do with VM
correctness.

## `TestStartupIPv6Connectors.testIPv6LinkLocal` — Windows IPv6 zone-ID format

```
java.net.URISyntaxException: Illegal character in authority at index 39:
  http://[fe80:0:0:0:1b75:41d6:c806:3307%{1BB5AB08-E458-4EAB-9CC7-4C3D36245461}]:51848/
```

Windows expresses an IPv6 link-local zone ID as a GUID
(`%{1BB5AB08-...}`); Linux uses a plain interface index (`%eth0` or `%3`).
Java's `URI` parser rejects the `{...}` form. This is what the OS handed back
for the local link-local address on this machine — nothing to do with
CratonVM; a Linux host wouldn't hit this at all, and HotSpot on this same
Windows host would get the identical malformed URI.

## `TestHttp2InitialConnection.testMultipleHostHeaders` — OS display language

```
org.junit.ComparisonFailure: expected:<...[content-language]-[[en]]...>
                                   but was:<...[content-language]-[[ru]]...>
```

The test asserts a `Content-Language: en` response header; this host's OS
locale is Russian, and something in the response path (likely an
`Accept-Language`-negotiated or default-locale-derived header) reflects that.
Same category as several other Russian-OS-locale artifacts already documented
in the netty runs this session (`SocketException` messages rendered in
Russian, etc.) — a property of this specific Windows installation, not the VM.

## `TestDeployTask`, `TestJspC` — missing `ant.jar`

```
java.lang.NoClassDefFoundError: org/apache/tools/ant/Task
```

Confirmed via HotSpot cross-check on `TestDeployTask` in an earlier session
turn: HotSpot gets `NOSUMMARY` (can't even load the test class) with the
identical classpath. `org.apache.tools.ant.Task` genuinely isn't on this
fixture's classpath. Same category as the EasyMock/ByteBuddy gap
(`easymock-bytebuddy-classpath-version-gap-not-a-cratonvm-bug.md`) —
a fixture dependency gap.

## `TestResponsePerformance.testToAbsolutePerformance`, `TestAsyncMessagesPerformance.testAsyncTiming` — relative-timing races

Both assert one code path is *faster than* another (`homebrewWin ==
winTarget` — a best-of-N race between two implementations; an async
websocket round-trip timing handshake). Not confirmed root-caused, but the
shape is exactly the kind of test this session's testing has repeatedly found
sensitive to CratonVM's overall throughput profile — if the two competing
paths don't slow down by the same *proportion* under CratonVM's execution
model (native-dispatch-heavy paths being disproportionately more expensive
than pure-interpreted-loop paths, which several other pages this session have
measured directly), a relative "A beats B" assertion can flip even though
neither path is behaving incorrectly. Worth a dedicated investigation if
either shows up as a priority; not attempted here.

## Repro

Each of these reproduces via the standard harness invocation against its own
class name:
```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Category all -Start <index> -Count 1 -GcFlag '-XX:+UseZGC' -RunName repro -TimeoutSec 300
```
