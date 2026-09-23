@echo off
REM One-off release build for the G67-1/G68-1/G78-1 retirement session.
REM Uses its own CARGO_TARGET_DIR so it never clobbers another worktree's binary.
cd /d "%~dp0..\.."
call "%~dp0..\find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=target-g67g68g78
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli %*
