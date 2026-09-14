@echo off
REM CPU build for the main checkout (recreated after scripts cleanup; cf. worktree build-wt.bat).
cd /d "%~dp0.."
call "%~dp0find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli %*
