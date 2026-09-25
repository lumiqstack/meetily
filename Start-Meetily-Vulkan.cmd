@echo off
setlocal
tasklist /FI "IMAGENAME eq meetily.exe" /NH | find /I "meetily.exe" >nul
if not errorlevel 1 exit /b 0
set "MEETILY_LLAMA_HELPER=D:\mvk4276454\release\llama-helper.exe"
set "VULKAN_SDK=D:\apps\VulkanSDK\1.4.304.0"
set "PATH=%VULKAN_SDK%\Bin;%PATH%"
start "" /D "D:\codex\meetily-0.4.0\target\release" "D:\codex\meetily-0.4.0\target\release\meetily.exe"
endlocal
