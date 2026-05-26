@echo off
REM Drop-in `java` replacement that delegates to CratonVM with the JDK
REM auto-discovered via env var. Used by Maven Surefire's -Djvm flag so
REM test forks run under CratonVM with no per-invocation --java-home noise.
set "CRATONVM_JAVA_HOME=C:\Program Files\Java\jdk-25"
"C:\craton\CratonVM\target\release\cratonvm.exe" %*
