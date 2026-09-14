@echo off
REM CPU release build. Uses the standard cargo/rustc on PATH by default. To build
REM with a custom (e.g. renamed, kill-proof under multi-session taskkill)
REM toolchain, set the CARGO and/or RUSTC environment variables before invoking:
REM   set RUSTC=C:\path\to\custom-rustc.exe
REM   set CARGO=C:\path\to\custom-cargo.exe
cd /d "%~dp0"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
if not defined RUSTC set "RUSTC=rustc"
if not defined CARGO set "CARGO=cargo"
"%CARGO%" build --release -p cratonvm-cli %*
