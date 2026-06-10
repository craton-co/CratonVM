@echo off
REM Build the 'java' bin (same main.rs as cratonvm) -> target/release/java.exe.
REM Used as the after-EC-fix CPU binary; sidesteps the locked cratonvm.exe.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo build --release -p cratonvm-cli --bin java --features java-bin-alias
