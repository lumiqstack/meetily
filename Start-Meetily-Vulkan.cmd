@echo off
setlocal
rem Launch the locally built Meetily with the Vulkan summary helper.
rem
rem Machine-specific paths are not kept in the repository. Put them in
rem Start-Meetily-Vulkan.local.cmd next to this file (gitignored); it may set:
rem   MEETILY_LLAMA_HELPER  full path to the llama-helper.exe to use
rem   VULKAN_SDK            Vulkan SDK root; its Bin folder is added to PATH
rem   MEETILY_EXE           app executable (default: target\release\meetily.exe here)
rem scripts\windows\build-vulkan.ps1 creates that file when it is missing.

tasklist /FI "IMAGENAME eq meetily.exe" /NH | find /I "meetily.exe" >nul
if not errorlevel 1 exit /b 0

if exist "%~dp0Start-Meetily-Vulkan.local.cmd" call "%~dp0Start-Meetily-Vulkan.local.cmd"
if not defined MEETILY_EXE set "MEETILY_EXE=%~dp0target\release\meetily.exe"
if not exist "%MEETILY_EXE%" (
  echo Meetily executable not found: %MEETILY_EXE%
  pause
  exit /b 1
)
if defined MEETILY_LLAMA_HELPER if not exist "%MEETILY_LLAMA_HELPER%" (
  echo Summary helper not found: %MEETILY_LLAMA_HELPER%
  pause
  exit /b 1
)
if defined VULKAN_SDK set "PATH=%VULKAN_SDK%\Bin;%PATH%"
for %%I in ("%MEETILY_EXE%") do set "MEETILY_DIR=%%~dpI"
start "" /D "%MEETILY_DIR%" "%MEETILY_EXE%"
endlocal
