@echo off
echo === A: target/release (NO fix, baseline) ===
C:\craton\CratonVM\target\release\cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --nojit --stack-dump-on-timeout 600 -Xmx32m -cp "C:\craton\CratonVM" DecChurn 2000000 > C:\craton\CratonVM\decchurn_old.log 2>&1
echo A_EXIT=%ERRORLEVEL%>> C:\craton\CratonVM\decchurn_old.log
echo === B: target/release-with-debug (WITH pin fix) ===
C:\craton\CratonVM\target\release-with-debug\cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --nojit --stack-dump-on-timeout 600 -Xmx32m -cp "C:\craton\CratonVM" DecChurn 2000000 > C:\craton\CratonVM\decchurn_fix.log 2>&1
echo B_EXIT=%ERRORLEVEL%>> C:\craton\CratonVM\decchurn_fix.log
echo ALL_DONE
