@echo off
REM SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
REM Copyright 2024-2026 Craton Software Company

REM GPU build for CratonVM (cratonvm bin, gpu-driver feature) into target-gpu.
cd /d "%~dp0.."
call "%~dp0find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver --target-dir target-gpu
