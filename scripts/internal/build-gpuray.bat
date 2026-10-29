@echo off
REM Scratch GPU build for the raytracer-divergence worktree: cratonvm bin with
REM the gpu-driver feature, into an isolated target dir so it never collides
REM with another worktree's target-gpu.
cd /d "%~dp0.."
call "%~dp0find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver --target-dir target-gpuray
