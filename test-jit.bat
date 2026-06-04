@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
cargo test -p cratonvm-jit --test intrinsic_int_bits --test intrinsic_long_bits %*
