@echo off
REM Fast cargo check via RENAMED cargo/rustc (kill-proof) to confirm dev compiles.
cd /d "%~dp0"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
set RUSTC=C:\Users\Victor\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\rsc_cv20.exe
"C:\Users\Victor\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cgo_cv20.exe" check --release -p cratonvm-vm
