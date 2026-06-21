@echo off
REM Incremental release build for the MAIN dev checkout (verify cherry-picked fixes compile).
REM Uses the standard cargo/rustc on PATH by default; set CARGO and/or RUSTC to
REM override (e.g. a renamed, kill-proof toolchain). The cargo registry root is
REM taken from CARGO_HOME (default %USERPROFILE%\.cargo), and the libffi-sys
REM include dirs are discovered there and prepended to INCLUDE before vcvars.
cd /d "%~dp0"
if not defined CARGO_HOME set "CARGO_HOME=%USERPROFILE%\.cargo"
set "INCLUDE="
for /d %%D in ("%CARGO_HOME%\registry\src\*\libffi-sys-*") do call :addffi "%%~fD"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
if not defined RUSTC set "RUSTC=rustc"
if not defined CARGO set "CARGO=cargo"
"%CARGO%" build --release -p cratonvm-cli %*
goto :eof

:addffi
set "FFI=%~1"
set "INCLUDE=%INCLUDE%%FFI%\libffi;%FFI%\libffi\include;%FFI%\include\msvc;%FFI%\libffi\src\x86;"
goto :eof
