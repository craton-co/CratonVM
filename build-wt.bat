@echo off
REM Isolated worktree build for CratonVM-wfenum (fix/wildfly-enum-constants).
cd /d "%~dp0"
REM vcvars64 sets LIB/INCLUDE/PATH so the MSVC linker is used; then we UNSET
REM VCINSTALLDIR + VSCMD_ARG_TGT_ARCH (libffi-sys rerun-if-env-changed list) so the
REM seeded prebuilt libffi.lib stays valid and cargo skips the failing build script.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli %*
