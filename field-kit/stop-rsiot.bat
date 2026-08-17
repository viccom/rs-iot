@echo off
REM ====================================================================
REM  rs-iot field stopper - graceful stop-all via warden API, then kill
REM  warden daemon (Job Object tears down the whole tree, no orphans).
REM  ASCII + CRLF only. Linear goto flow.
REM ====================================================================
setlocal

set "WAPI=http://127.0.0.1:8789/api/v1"

echo [1/2] stopping all services (graceful) ...
curl -s -m 10 -X POST %WAPI%/services/stop-all >nul 2>&1
ping -n 3 127.0.0.1 >nul

echo [2/2] killing warden daemon ...
taskkill /IM warden.exe /F >nul 2>&1

echo rs-iot field stopped.
endlocal
exit /b 0
