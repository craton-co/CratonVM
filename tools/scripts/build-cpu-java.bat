@echo off
REM Build the 'java' bin (same main.rs as cratonvm) -> target/release/java.exe.
REM Used as the after-EC-fix CPU binary; sidesteps the locked cratonvm.exe.
cd /d "%~dp0.."
call "%~dp0find-vcvars.bat" || exit /b 1
call "%VCVARS64%"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli --bin java --features java-bin-alias
