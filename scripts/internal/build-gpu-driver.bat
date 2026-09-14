@echo off
rem Build cratonvm with --features gpu-driver into target-gpu (separate from main target dir).
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
set "VCINSTALLDIR= "
set "VSCMD_ARG_TGT_ARCH= "
set "PATH=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64;%PATH%;C:\Program Files\Git\usr\bin"
set "CARGO_TARGET_DIR=C:\craton\CratonVM\target-gpu"
cd /d C:\craton\CratonVM
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver
