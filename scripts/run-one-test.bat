@echo off
REM Run a single H2 test class on the worktree build. %1 = fully-qualified class, %2 = extra flag (e.g. --nojit)
cd /d C:\craton\CratonVM\apps\h2database\h2
set CP=temp;ext\jts-core-1.19.0.jar;ext\jakarta.servlet-api-5.0.0.jar;ext\javax.servlet-api-4.0.1.jar;ext\asm-9.5.jar;ext\lucene-core-9.7.0.jar;ext\lucene-analysis-common-9.7.0.jar;ext\lucene-queryparser-9.7.0.jar;ext\slf4j-api-2.0.7.jar;ext\junit-jupiter-api-5.10.0.jar;ext\apiguardian-1.1.2.jar;ext\org.osgi.core-5.0.0.jar;ext\org.osgi.service.jdbc-1.1.0.jar
C:\craton\CratonVM-h2val\target\release\cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" %2 --stack-dump-on-timeout 0 -Xmx1g -cp "%CP%" %1
echo ONETEST_EXIT=%ERRORLEVEL%
