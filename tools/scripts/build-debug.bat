@echo off
REM release-with-debug build (same codegen as release + line-tables/symbols) for SEGV symbolization.
cd /d "%~dp0.."
call "%~dp0find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --profile release-with-debug -p cratonvm-cli --bin cratonvm
