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
| `DateFmtProbe` | Quantifies why `TestOneLineFormatterPerformance.testDateFormat`'s "the cache should beat `String.format`" assertion inverts on CratonVM: `java.util.Formatter.format` is a Rust intrinsic (~2x HotSpot) while `SimpleDateFormat.format` and even a bare `StringBuilder` are ordinary bytecode (~70-200x). Not a defect in the slower path. |

`RunMethods.java` is not a probe but the tool that isolated the first one: it
runs an **ordered subset** of a class's `@Test` methods in one JVM, which is how
"passes alone, fails in-class" ordering dependencies get bisected without
paying for the whole class. Drop it next to the compiled Tomcat tests (e.g.
`apps\tomcat\.suite`) and run
`RunMethods <fqcn> <method1> <method2> …`.
