@echo off
REM Isolated GPU build into target-fresh-gpu (-j 1, gpu-driver feature).
REM INTERNAL-DEV-ONLY: the VS BuildTools vcvars64.bat and Git usr/bin PATH below
REM are this machine's MSVC toolchain locations; adjust for your environment.
cd /d "%~dp0.."
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver --target-dir target-fresh-gpu -j 1
