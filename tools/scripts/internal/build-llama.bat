@echo off
REM Scratch release build for the GPULlama3 model-load worktree, into an
REM isolated target dir so it never collides with another worktree's target.
cd /d "%~dp0.."
call "%~dp0find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli --bin cratonvm --target-dir target-llama
