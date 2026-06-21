@echo off
REM Fast cargo check to confirm dev compiles. Uses the standard cargo/rustc on
REM PATH by default. To build with a custom (e.g. renamed, kill-proof) toolchain,
REM set the CARGO and/or RUSTC environment variables before invoking, e.g.:
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
"%CARGO%" check --release -p cratonvm-vm
