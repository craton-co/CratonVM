@echo off
setlocal EnableExtensions
call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" >nul
set "HIBFIVE_FFI=C:\Users\Victor\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\libffi-sys-2.3.0"
set "INCLUDE=%INCLUDE%;%HIBFIVE_FFI%\libffi;%HIBFIVE_FFI%\libffi\include;%HIBFIVE_FFI%\include\msvc;%HIBFIVE_FFI%\libffi\src\x86"
set "CARGO_TARGET_DIR=C:\craton\CratonVM-hibfive-takeover-20260730\target-hibfive-takeover"
pushd "C:\craton\CratonVM-hibfive-takeover-20260730"
cargo build --release -p cratonvm-cli --bin cratonvm > "C:\craton\CratonVM-hibfive-takeover-20260730\.hibtake\build.log" 2>&1
set "HIBFIVE_EXIT=%ERRORLEVEL%"
popd
exit /b %HIBFIVE_EXIT%
