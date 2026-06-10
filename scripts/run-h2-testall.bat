@echo off
REM Run H2 org.h2.test.TestAll on the isolated cratonvm build.
REM Arg %1 selects JIT mode log suffix; env is set by caller.
cd /d C:\craton\CratonVM\apps\h2database\h2
set CP=temp;ext\jts-core-1.19.0.jar;ext\jakarta.servlet-api-5.0.0.jar;ext\javax.servlet-api-4.0.1.jar;ext\asm-9.5.jar;ext\lucene-core-9.7.0.jar;ext\lucene-analysis-common-9.7.0.jar;ext\lucene-queryparser-9.7.0.jar;ext\slf4j-api-2.0.7.jar;ext\junit-jupiter-api-5.10.0.jar;ext\apiguardian-1.1.2.jar;ext\org.osgi.core-5.0.0.jar;ext\org.osgi.service.jdbc-1.1.0.jar
C:\craton\CratonVM\target-h2\release\cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --stack-dump-on-timeout 0 -Xmx1g -cp "%CP%" org.h2.test.TestAll
echo TESTALL_EXIT=%ERRORLEVEL%
