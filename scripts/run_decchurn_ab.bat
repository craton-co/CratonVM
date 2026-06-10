@echo off
REM A/B DecChurn: baseline (target/release) vs fixed (target/release-with-debug).
REM Repo root derives from %~dp0; JAVA_HOME is honoured when set.
setlocal
set "REPO=%~dp0.."
if not defined JAVA_HOME set "JAVA_HOME=C:\Program Files\Java\jdk-25"
echo === A: target/release (NO fix, baseline) ===
"%REPO%\target\release\cratonvm.exe" --java-home "%JAVA_HOME%" --nojit --stack-dump-on-timeout 600 -Xmx32m -cp "%REPO%" DecChurn 2000000 > "%REPO%\decchurn_old.log" 2>&1
echo A_EXIT=%ERRORLEVEL%>> "%REPO%\decchurn_old.log"
echo === B: target/release-with-debug (WITH pin fix) ===
"%REPO%\target\release-with-debug\cratonvm.exe" --java-home "%JAVA_HOME%" --nojit --stack-dump-on-timeout 600 -Xmx32m -cp "%REPO%" DecChurn 2000000 > "%REPO%\decchurn_fix.log" 2>&1
echo B_EXIT=%ERRORLEVEL%>> "%REPO%\decchurn_fix.log"
echo ALL_DONE
endlocal
