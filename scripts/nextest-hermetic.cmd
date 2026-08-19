@echo off
setlocal EnableExtensions DisableDelayedExpansion

rem Keep this allowlist in sync with scripts/nextest-hermetic.sh.
set "LUCHTA_HERMETIC_TEST_CANARY=present"
for /f "delims==" %%V in ('set') do call :filter_environment "%%V"
set "HERMETIC_ENV_NAME="
set "LUCHTA_TEST_HERMETIC_WRAPPER=1"

%*
exit /b %ERRORLEVEL%

:filter_environment
set "HERMETIC_ENV_NAME=%~1"

if /i "%HERMETIC_ENV_NAME%"=="PATH" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="HOME" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="USERPROFILE" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="SYSTEMROOT" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="WINDIR" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="COMSPEC" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="PATHEXT" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="TMPDIR" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="TMP" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="TEMP" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="PWD" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="LLVM_PROFILE_FILE" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="LUCHTA_TEST_RCLONE" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="CARGO" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="CARGO_MANIFEST_DIR" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="CARGO_TARGET_TMPDIR" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="NEXTEST" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="NEXTEST_RUN_ID" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="NEXTEST_PROFILE" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="NEXTEST_VERSION" exit /b 0
if /i "%HERMETIC_ENV_NAME%"=="NEXTEST_WORKSPACE_ROOT" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,10%"=="CARGO_PKG_" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,14%"=="CARGO_BIN_EXE_" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,16%"=="NEXTEST_BIN_EXE_" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,11%"=="NEXTEST_LD_" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,13%"=="NEXTEST_DYLD_" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,3%"=="LD_" exit /b 0
if /i "%HERMETIC_ENV_NAME:~0,5%"=="DYLD_" exit /b 0

set "%HERMETIC_ENV_NAME%="
exit /b 0
