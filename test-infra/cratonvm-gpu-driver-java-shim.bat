@echo off
set "CRATONVM_JAVA_HOME=C:\Program Files\Java\jdk-25"
"C:\craton\CratonVM\target-gpu\release\cratonvm.exe" --gpu %*
