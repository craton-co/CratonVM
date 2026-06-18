@echo off
cd /d C:\craton\CratonVM
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
set RUST_MIN_STACK=536870912
set JAVA_HOME=C:\Program Files\Java\jdk-25
set PATH=%PATH%;C:\Program Files\Git\usr\bin
cargo test --release -p cratonvm-vm --test nested_clinit_startup --test iface_static_final_init -- --nocapture
echo TESTS_EXIT=%ERRORLEVEL%
