@echo off
REM A2 over-sized-object clamp fix build. Unique output -> cvma2clamp.exe.
cd /d "%~dp0"
set "FFI1=C:\Users\Admin\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\libffi-sys-2.3.0"
set "INCLUDE=%FFI1%\libffi;%FFI1%\libffi\include;%FFI1%\include\msvc;%FFI1%\libffi\src\x86"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
"C:\Users\Admin\.cargo\bin\cargo.exe" build --release -p cratonvm-cli %*
if exist "target\release\cratonvm.exe" copy /Y "target\release\cratonvm.exe" "cvma2clamp.exe"
