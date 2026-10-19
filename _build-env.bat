@echo off
set VSLANG=1033
call "C:/Program Files/Microsoft Visual Studio/18/Community/VC/Auxiliary/Build/vcvars64.bat" >nul
rem 用脚本自身目录，而不是写死某一份工作副本：workspace / 原目录都能直接构建。
cd /d "%~dp0"
set "FFMPEG_DIR=%~dp0ffmpeg-n9.0-latest-win64-lgpl-shared-9.0"
set "LIBCLANG_PATH=%~dp0_llvm\bin"
rem vcvars SDK detection needs reg.exe (blocked by sandbox); append SDK paths manually.
set "SDKINC=C:/Program Files (x86)\Windows Kits\10\Include\10.0.28000.0"
set "SDKLIB=C:/Program Files (x86)\Windows Kits\10\Lib\10.0.28000.0"
set "SDKBIN=C:/Program Files (x86)\Windows Kits\10\bin\10.0.28000.0\x64"
set "PATH=%PATH%;%SDKBIN%"
set "INCLUDE=%INCLUDE%;%SDKINC%\ucrt;%SDKINC%\shared;%SDKINC%\um;%SDKINC%\winrt"
set "LIB=%LIB%;%SDKLIB%\ucrt\x64;%SDKLIB%\um\x64"
rem ---- build hygiene: disable incremental compilation (2026-10-17) ----
rem Cargo keeps two incremental sessions per crate and deletes the older one on
rem EVERY build; this crate's session is 200-280 MB (.o + .pre-lto.bc), and
rem target\debug\incremental measured 762 MB / 31 dirs.
rem On this machine a delete is redirected to the Recycle Bin (verified), so each
rem cargo test / build drops a fresh batch of temp files there and the space is
rem not released until the bin is emptied.
rem With CARGO_INCREMENTAL=0 the incremental\ tree is never created at all; this
rem also avoids the sandbox write denial on that path (which makes rustc panic
rem with os error 5).
rem Cost: a one-line change rebuilds the whole crate (~40-60 s here).
set CARGO_INCREMENTAL=0
echo CARGO_INCREMENTAL=%CARGO_INCREMENTAL%
echo FFMPEG_DIR=%FFMPEG_DIR%
echo INCLUDE=%INCLUDE%
cargo %*
