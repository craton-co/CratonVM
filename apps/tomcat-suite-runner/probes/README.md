# Standalone reproducers for Tomcat-suite-found CratonVM bugs

Each of these replaces a multi-hour Tomcat test class with a few seconds of
plain Java. They need no Tomcat fixture and no classpath beyond themselves —
compile with any JDK and run under both VMs to compare:

```powershell
javac -d out probes\*.java
& $craton -Xmx2g -cp out <ProbeName>
& "$env:JAVA_HOME\bin\java.exe" -Xmx2g -cp out <ProbeName>
```

Run the CratonVM side with the suite's env so the same native registrations are
active as in the suite (see `run-tomcat-suite.md` §5 and
`run-tomcat-suite.ps1`'s `Invoke-Mode`):
`CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_ROOTSNAP_CACHE=1`.

| probe | bug it isolates |
|---|---|
| `JarRedeployProbe` | Jar/WAR byte caches keyed on path only, so replacing an archive in place at the same path serves the OLD content through `jar:file:…!/…` URLs (`JarFile` was unaffected — the asymmetry is the point). Root cause of `TestHostConfigAutomaticDeploymentUnpackWAR.testUnpackWARTTF`. Prints `OK` or `STALE` per access path. |
| `SetLastModProbe` | `java.io.File.setLastModified` returned `false` for **directories** (worked for files). Root cause of all four `TestHostConfigAutomaticDeploymentUpdateWarOffline` failures. |
| `FileStateProbe` | Control for the above two: confirms `exists()`/`isDirectory()`/`lastModified()`/`length()` are NOT cached across delete/recreate, so a stale-stat explanation can be ruled out. Expect `ALL OK` on both VMs. |
| `CharAtProbe` | Per-call cost of `String.charAt`/`String.length` inside an OSR-compiled loop, against a raw `char[]` load as the floor. The rung that found the inert String JIT intrinsics: 408 ns and 28.2 ns against HotSpot's 0.6 and ~0, while the `char[]` load was already at 1.0 ns — so the compiled loop body was fine and only the CALL was not. 1.6 / 1.1 ns after the fix. |
| `BufAccessorProbe` | Per-call cost of the single-element NIO buffer accessors Tomcat's `Utf8Encoder` slow path uses — `StringCharBuffer.get()` (via `CharBuffer.wrap(String)`, which is read-only and so has no array) and `HeapByteBuffer.put(byte)`. These two are 78 % of the WebSocket text send path. Also prints each buffer's concrete class and `hasArray()`, which is what selects `encodeNotHasArray` in the first place. |
| `BufCostProbe` | Decomposes the previous one: a plain user virtual call is the call-overhead floor, and the buffer accessors differ only in how many by-name field resolutions their native body performs. This is the rung that showed the cost is the ~90 ns native-dispatch floor and NOT the hierarchy walk — `bb.capacity()`, one field read, is already 89 ns against a 17 ns user virtual call. |
| `DateFmtProbe` | Quantifies why `TestOneLineFormatterPerformance.testDateFormat`'s "the cache should beat `String.format`" assertion inverts on CratonVM: `java.util.Formatter.format` is a Rust intrinsic (~2x HotSpot) while `SimpleDateFormat.format` and even a bare `StringBuilder` are ordinary bytecode (~70-200x). Not a defect in the slower path. |

`websocket/` holds the three files that reproduce
`TestWebSocketFrameClient#testConnectToServerEndpoint` in a form that reports
what the test itself cannot. Unlike the probes above these DO need the Tomcat
fixture and its classpath, because they reuse the suite's own
`WebSocketBaseTest`/`TesterFirehoseServer` — compile them with
`.suite\cp.txt` on the classpath and run the result through
`run-one.ps1 -Class org.apache.tomcat.websocket.FirehoseProbe2 -ExtraCp <dir>`.

* `FirehoseProbe` — the stock test plus a monitor thread printing the running
  message count. Answers "is the stream stopping or just slow?", which the
  test's single end-of-run assertion cannot.
* `ProbeFirehoseServer` + `FirehoseProbe2` — the same thing with a second
  `@ServerEndpoint` that reports its OWN send progress, so a slow SEND is told
  apart from a slow RECEIVE in one run, and with `-Dprobe.count=N` to shrink
  the 100000-message default to something that iterates in seconds. `recv`
  tracking `sent` to within ~10 messages for a whole run is what ruled out the
  entire "truncation" family.

`RunMethods.java` is not a probe but the tool that isolated the first one: it
runs an **ordered subset** of a class's `@Test` methods in one JVM, which is how
"passes alone, fails in-class" ordering dependencies get bisected without
paying for the whole class. Drop it next to the compiled Tomcat tests (e.g.
`apps\tomcat\.suite`) and run
`RunMethods <fqcn> <method1> <method2> …`.
