@echo off
rem A4 fork6-helper-window build — direct MSVC env (bypass vcvars64, which hangs
rem under concurrent sessions), own target dir, unique output binary name.
set "MSVC=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207"
set "SDK=C:\Program Files (x86)\Windows Kits\10"
set "SDKVER=10.0.26100.0"
set "INCLUDE=%MSVC%\include;%SDK%\Include\%SDKVER%\ucrt;%SDK%\Include\%SDKVER%\um;%SDK%\Include\%SDKVER%\shared;%SDK%\Include\%SDKVER%\winrt"
set "LIB=%MSVC%\lib\x64;%SDK%\Lib\%SDKVER%\ucrt\x64;%SDK%\Lib\%SDKVER%\um\x64"
set "PATH=%MSVC%\bin\Hostx64\x64;%SDK%\bin\%SDKVER%\x64;C:\Users\Victor\.cargo\bin;C:\Windows\System32;C:\Windows;C:\Program Files\Git\usr\bin"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set "CARGO_TARGET_DIR=C:\craton\CratonVM-oldgc-20260703\target"
cd /d C:\craton\CratonVM-oldgc-20260703
echo [%date% %time%] build start >> build-oldgc.log
cargo build --release -p cratonvm-cli --bin cratonvm >> build-oldgc.log 2>&1
echo [%date% %time%] build exit %errorlevel% >> build-oldgc.log
if %errorlevel%==0 copy /Y target\release\cratonvm.exe target\release\cvmp-mapfix2-20260703.exe >> build-oldgc.log 2>&1
