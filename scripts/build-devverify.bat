@echo off
cd /d "%~dp0"
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set "PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin"
set "RUSTC=C:\Users\Victor\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\hmrustc.exe"
"C:\Users\Victor\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\hmcargo.exe" build --release -p cratonvm-cli --bin cratonvm
echo BUILD_EXIT_CODE=%errorlevel%
