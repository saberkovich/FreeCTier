@echo off
setlocal
rem Usage: build.bat [--debug] [--no-pause]
set "FCT_PAUSE=1"
for %%A in (%*) do if /I "%%~A"=="--no-pause" set "FCT_PAUSE=0"
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\build.ps1" %*
set "FCT_EXIT=%ERRORLEVEL%"
if not "%FCT_EXIT%"=="0" echo Build failed. See the error above.
if "%FCT_PAUSE%"=="1" pause
exit /b %FCT_EXIT%
