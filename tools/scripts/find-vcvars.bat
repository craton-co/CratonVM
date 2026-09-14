@echo off
REM find-vcvars.bat — locate this machine's vcvars64.bat without hardcoding
REM one Visual Studio edition/year/install path.
REM
REM Sets %VCVARS64% to the discovered path and returns 0, or prints an error
REM and returns 1 if no MSVC x64 toolchain could be found.
REM
REM Usage (from another script):
REM   call "%~dp0find-vcvars.bat" || exit /b 1
REM   call "%VCVARS64%" >nul 2>&1

set "VCVARS64="

REM 1) vswhere.exe ships with the Visual Studio Installer since VS2017 at this
REM    fixed, edition-independent location — the canonical way to ask "where
REM    is *any* installed VS with the C++ x86/x64 tools", instead of guessing
REM    a specific year/edition path.
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
    for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do (
        if exist "%%i\VC\Auxiliary\Build\vcvars64.bat" set "VCVARS64=%%i\VC\Auxiliary\Build\vcvars64.bat"
    )
)

REM 2) Fallback: check the handful of common default install paths directly,
REM    in case vswhere itself is unavailable (rare, but not universal).
if not defined VCVARS64 (
    for %%P in (
        "%ProgramFiles%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
        "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
        "%ProgramFiles%\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
        "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
        "%ProgramFiles%\Microsoft Visual Studio\2022\Professional\VC\Auxiliary\Build\vcvars64.bat"
        "%ProgramFiles%\Microsoft Visual Studio\2022\Enterprise\VC\Auxiliary\Build\vcvars64.bat"
    ) do (
        if not defined VCVARS64 if exist %%P set "VCVARS64=%%~P"
    )
)

if not defined VCVARS64 (
    echo error: could not locate vcvars64.bat ^(no vswhere.exe result and no
    echo        common install path matched^). Install the "Desktop development
    echo        with C++" workload, or set VCVARS64 yourself before running
    echo        this script.
    exit /b 1
)

exit /b 0
