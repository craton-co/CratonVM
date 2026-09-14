@echo off
REM Round-9 cross-cutting HIGH-3: profile-guided optimization (PGO) recipe.
REM
REM Windows companion to scripts/pgo.sh. Same four-phase flow:
REM   1. instrumented build  (profile-generate)
REM   2. run bench suite     (collects .profraw shards)
REM   3. merge profiles      (llvm-profdata)
REM   4. optimized rebuild   (profile-use)
REM
REM Requirements:
REM   - llvm-tools-preview rustup component:
REM       rustup component add llvm-tools-preview
REM   - llvm-profdata.exe reachable on PATH (under
REM       %USERPROFILE%\.rustup\toolchains\<toolchain>\lib\rustlib\<host>\bin)
REM
REM Usage:
REM   scripts\pgo.cmd

setlocal enableextensions enabledelayedexpansion

if "%PROFILE_DIR%"=="" set PROFILE_DIR=%TEMP%\cratonvm-pgo
set TARGET_FEATURES=+sse4.2,+pclmulqdq

echo [pgo] resetting profile directory: %PROFILE_DIR%
if exist "%PROFILE_DIR%" rmdir /s /q "%PROFILE_DIR%"
mkdir "%PROFILE_DIR%"

echo [pgo] phase 1/4: instrumented build
set RUSTFLAGS=-C profile-generate=%PROFILE_DIR% -C target-feature=%TARGET_FEATURES%
cargo build --release --workspace
if errorlevel 1 goto :fail

echo [pgo] phase 2/4: collect profile data via bench suite
cargo bench --workspace -- --quick

echo [pgo] phase 3/4: merge profiles
llvm-profdata merge -o "%PROFILE_DIR%\merged.profdata" "%PROFILE_DIR%"
if errorlevel 1 goto :fail

echo [pgo] phase 4/4: optimized rebuild
set RUSTFLAGS=-C profile-use=%PROFILE_DIR%\merged.profdata -C llvm-args=-pgo-warn-missing-function -C target-feature=%TARGET_FEATURES%
cargo build --release --workspace
if errorlevel 1 goto :fail

echo [pgo] done; binaries in target\release\ now PGO-optimized
endlocal
exit /b 0

:fail
echo [pgo] FAILED
endlocal
exit /b 1
