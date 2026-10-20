@echo off
REM Targeted test runner for the keycloak-gap5 worktree (isolated target + renamed toolchain).
cd /d "%~dp0.."
set "FFI1=C:\Users\Victor\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\libffi-sys-2.3.0"
set "FFI2=C:\Users\Victor\.cargo\registry\src\index.crates.io-6f17d22bba15001f\libffi-sys-2.3.0"
set "INCLUDE=%FFI1%\libffi;%FFI1%\libffi\include;%FFI1%\include\msvc;%FFI1%\libffi\src\x86;%FFI2%\libffi;%FFI2%\libffi\include;%FFI2%\include\msvc;%FFI2%\libffi\src\x86"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set CARGO_TARGET_DIR=
set RUST_MIN_STACK=536870912
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
set "RUSTC=C:\Users\Victor\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\kcrustc.exe"
"C:\Users\Victor\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\kccargo.exe" test --release --target-dir target-kcboot %*
