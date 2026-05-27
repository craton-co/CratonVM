@echo off
REM Drop-in `java` replacement that delegates to TornadoVM-bundled JDK 25.0.3
REM with the TornadoVM @argfile (Graal + tornado.runtime modules + PTX driver).
REM Used by Maven Surefire's -Djvm flag so tests run with Graal-as-JIT
REM instead of HotSpot C2.
"C:\craton\tornadovm\jdk-25.0.3\bin\java.exe" "@C:\craton\tornadovm\tornadovm-4.0.1-jdk25-ptx\tornado-argfile" %*
