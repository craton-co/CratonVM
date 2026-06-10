@echo off
REM Dev-verify build. Uses the standard cargo/rustc on PATH by default.
REM To build with a custom toolchain (e.g. a wrapped rustc/cargo), set the
REM CARGO and/or RUSTC environment variables before invoking, e.g.:
REM   set RUSTC=C:\path\to\custom-rustc.exe
REM   set CARGO=C:\path\to\custom-cargo.exe
cd /d "%~dp0\.."
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set "PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin"
if not defined CARGO set "CARGO=cargo"
"%CARGO%" build --release -p cratonvm-cli --bin cratonvm
echo BUILD_EXIT_CODE=%errorlevel%
