@echo off
REM Isolated CPU build for CratonVM into a PRIVATE target dir (target-h2) to
REM avoid lock/output contention with concurrent builds from other sessions
REM sharing the default target/. Seeded from target/release for incremental speed.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set VCINSTALLDIR=
set VSCMD_ARG_TGT_ARCH=
set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin
set CARGO_TARGET_DIR=C:\craton\CratonVM\target-h2
cargo build --release -p cratonvm-cli --bin cratonvm
