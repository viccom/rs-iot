@echo off
REM ====================================================================
REM  rs-iot field launcher (warden-managed, E:\rsiot-field)
REM  Usage:  double-click start-rsiot.bat
REM  Stop:   run stop-rsiot.bat  (warden daemon keeps services alive
REM          after this bat exits)
REM
REM  Encoding: ASCII + CRLF only. cmd parser mis-parses UTF-8/LF bats.
REM  Linear goto flow, no parenthesized blocks. Delays via ping,
REM  matching via findstr (avoid GNU timeout/find PATH shadowing).
REM  Depends on: curl (Win10 builtin), jq, powershell
REM ====================================================================
setlocal enabledelayedexpansion

set "ROOT=%~dp0"
set "ROOT=%ROOT:~0,-1%"

echo ========================================
echo  rs-iot field start (warden orchestrated)
echo  ROOT=%ROOT%
echo ========================================

set "WAPI=http://127.0.0.1:8789/api/v1"

REM === 0. generate warden services.toml from template (idempotent, root-portable) ===
powershell -NoProfile -ExecutionPolicy Bypass -Command "$t=[IO.File]::ReadAllText('%ROOT%\warden\config\services.toml.template').Replace('__ROOT__','%ROOT:\=/%');[IO.File]::WriteAllText('%ROOT%\warden\config\services.toml',$t)"
if not exist "%ROOT%\warden\config\services.toml" goto wcfg_fail
goto wcfg_ok

:wcfg_fail
echo [ERROR] failed to generate warden\config\services.toml from template.
pause
exit /b 1

:wcfg_ok
REM === 1. warden daemon (:8789) - start if not running ===
curl -s -m 2 %WAPI%/health 2>nul | findstr /c:"ok" >nul 2>&1
if not errorlevel 1 goto warden_ok
echo [1/5] starting warden daemon :8789 ...
start "warden-rsiot" /min /d "%ROOT%\warden" cmd /c "warden.exe run --config config\services.toml"
set /a tries=0

:wait_warden
set /a tries+=1
curl -s -m 2 %WAPI%/health 2>nul | findstr /c:"ok" >nul 2>&1
if not errorlevel 1 goto warden_ok
if !tries! geq 15 goto warden_fail
ping -n 2 127.0.0.1 >nul
goto wait_warden

:warden_fail
echo [ERROR] warden API not ready in 15s. Check port 8789 or warden window.
pause
exit /b 1

:warden_ok
echo [1/6] warden daemon ready.

REM === 2. simulator (factory device sim: Modbus TCP 502 / RTU COM3 / OPC UA 4840) ===
echo [2/6] starting simulator (devices) ...
curl -s -X POST %WAPI%/services/simulator/start >nul
ping -n 3 127.0.0.1 >nul

REM === 3. rs-iot main (:8790) - start and wait for login (health) ===
echo [3/6] starting rs-iot :8790 ...
curl -s -X POST %WAPI%/services/rs-iot/start >nul
set "JWT="
set /a tries=0

:get_jwt
set /a tries+=1
for /f "delims=" %%j in ('curl -s -m 2 "http://127.0.0.1:8790/api/v1/user/login" -H "content-type: application/json" -d "{\"user_id\":\"admin\",\"pass\":\"admin\"}" ^| jq -r .token 2^>nul') do set "JWT=%%j"
if not "!JWT!"=="" if not "!JWT!"=="null" goto got_jwt
if !tries! geq 30 goto jwt_fail
ping -n 2 127.0.0.1 >nul
goto get_jwt

:jwt_fail
echo [ERROR] rs-iot login failed in 30s. Check warden console http://127.0.0.1:8789
pause
exit /b 1

:got_jwt
echo [3/6] rs-iot up, JWT issued.

REM === 3. regenerate work/reasonix.toml with fresh JWT (idempotent) ===
echo [4/6] generating work/reasonix.toml ...
set "RSIOT_JWT=!JWT!"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$t=[IO.File]::ReadAllText('%ROOT%\work\reasonix.toml.template').Replace('__ROOT__','%ROOT:\=/%').Replace('__JWT__',$env:RSIOT_JWT);[IO.File]::WriteAllText('%ROOT%\work\reasonix.toml',$t)"
if errorlevel 1 goto cfg_fail
if not exist "%ROOT%\work\reasonix.toml" goto cfg_fail
echo [4/6] reasonix.toml ready.
goto cfg_ok

:cfg_fail
echo [ERROR] failed to generate reasonix.toml from template.
pause
exit /b 1

:cfg_ok
REM === 4. reasonix serve (:8787) - restart picks up fresh JWT on rerun ===
echo [5/6] starting reasonix serve :8787 ...
curl -s -X POST %WAPI%/services/reasonix/restart >nul 2>&1
curl -s -X POST %WAPI%/services/reasonix/start >nul 2>&1
set /a tries=0

:wait_serve
set /a tries+=1
curl -s -m 2 -o nul -w "%%{http_code}" "http://127.0.0.1:8787/status?token=test-serve-token-123" 2>nul | findstr /r "200 302 401" >nul 2>&1
if not errorlevel 1 goto serve_ok
if !tries! geq 20 goto serve_fail
ping -n 2 127.0.0.1 >nul
goto wait_serve

:serve_fail
echo [WARN] reasonix serve not responding in 20s. Check warden console logs.

:serve_ok
echo [5/6] reasonix serve up.

REM === 5b. set reasonix yolo mode (test field: smooth e2e, tools auto-approved) ===
set "CJ=%TEMP%\rsiot-field-cookie.txt"
curl -s -c "%CJ%" -X POST http://127.0.0.1:8787/auth/token -H "content-type: application/json" -d "{\"token\":\"test-serve-token-123\"}" >nul 2>&1
curl -s -b "%CJ%" -X POST http://127.0.0.1:8787/tool-approval-mode -H "content-type: application/json" -d "{\"mode\":\"yolo\"}" >nul 2>&1
del "%CJ%" >nul 2>&1
echo [5/6] reasonix set to yolo mode.

REM === 6. rsiot-gateway (:8080, frontend entry) ===
echo [6/6] starting rsiot-gateway :8080 ...
curl -s -X POST %WAPI%/services/rsiot-gateway/start >nul
set /a tries=0

:wait_gw
set /a tries+=1
curl -s -m 2 http://127.0.0.1:8080/api/gw/health 2>nul | findstr /c:"online" >nul 2>&1
if not errorlevel 1 goto gw_ok
if !tries! geq 15 goto gw_fail
ping -n 2 127.0.0.1 >nul
goto wait_gw

:gw_fail
echo [WARN] gateway health not green in 15s. Check warden console logs.

:gw_ok
echo.
echo ========================================
echo  rs-iot field is UP
echo    frontend+gateway : http://127.0.0.1:8080  (login admin/admin)
echo    warden console   : http://127.0.0.1:8789
echo    rs-iot REST      : http://127.0.0.1:8790
echo    reasonix serve   : http://127.0.0.1:8787
echo ========================================
echo  This window can be closed. Services are managed by warden.
endlocal
exit /b 0
