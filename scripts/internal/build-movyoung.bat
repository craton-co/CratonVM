@echo off
REM Release build for the default moving/compacting young gen work.
REM Output -> cvmmovyoung.exe (unique name, isolated worktree target dir).
cd /d "%~dp0"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set "PATH=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64;%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin"
"C:\Users\Victor\.cargo\bin\cargo.exe" build --release -p cratonvm-cli --bin cratonvm %*
if errorlevel 1 exit /b 1
copy /Y "target\release\cratonvm.exe" "cvmmovyoung.exe"
