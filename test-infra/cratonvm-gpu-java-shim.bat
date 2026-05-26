@echo off
REM CratonVM with --gpu flag for Maven Surefire forks. Eligible static
REM methods (analyzer-tagged, > --gpu-min-work threshold) are offloaded to
REM the CUDA device. Tests that fall under the threshold run on CPU
REM (interpreter or JIT). Same JDK 25 as the plain cratonvm shim.
set "CRATONVM_JAVA_HOME=C:\Program Files\Java\jdk-25"
"C:\craton\CratonVM\target\release\cratonvm.exe" --gpu %*
