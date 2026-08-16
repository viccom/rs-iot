# pack-field-kit.ps1 — 组装完整 Windows 端到端测试场发行包（含二进制）
#
# 用法（在 rs-iot 仓库根）：
#   powershell -NoProfile -ExecutionPolicy Bypass -File field-kit\pack-field-kit.ps1
# 可选参数（覆盖默认来源路径）：
#   -RsiotTarget E:\github.com\rs-iot\target\release
#   -WardenExe   E:\github.com\warden\target\release\warden.exe
#   -SimDist     E:\rsiot-field\simulator\dist
#   -Com0com     E:\rsiot-field\simulator\com0com
#   -ReasonixExe E:\rsiot-field\bin\reasonix.exe
#   -HomeDir     E:\rsiot-field\home        （REASONIX_HOME，含 .env 密钥——产物勿外发）
#
# 产物：field-kit-dist\（可直接 xcopy 到任意根目录后运行 start-rsiot.bat）
#       + field-kit-dist.zip

param(
    $RsiotTarget = "E:\github.com\rs-iot\target\release",
    $WardenExe   = "E:\github.com\warden\target\release\warden.exe",
    $SimDist     = "E:\rsiot-field\simulator\dist",
    $Com0com     = "E:\rsiot-field\simulator\com0com",
    $ReasonixExe = "E:\rsiot-field\bin\reasonix.exe",
    $HomeDir     = "E:\rsiot-field\home",
    $OutDir      = "field-kit-dist"
)

$ErrorActionPreference = "Stop"
$kit = Join-Path $PSScriptRoot $OutDir
if (Test-Path $kit) { Remove-Item $kit -Recurse -Force }

# 1. 配套资产（本目录 bats + config 模板）
foreach ($d in @("$kit\warden\config", "$kit\work", "$kit\bin", "$kit\home")) {
    New-Item -ItemType Directory -Force -Path $d | Out-Null
}
Copy-Item "$PSScriptRoot\start-rsiot.bat", "$PSScriptRoot\stop-rsiot.bat" $kit
Copy-Item "$PSScriptRoot\config\services.toml.template" "$kit\warden\config\"
Copy-Item "$PSScriptRoot\config\reasonix.toml.template" "$kit\work\"
Copy-Item "$PSScriptRoot\config\rsiot-config.toml" "$kit\config.toml"
# gateway 配置：example -> 实际 gateway.toml，__ROOT__ 替换为部署根（正斜杠）。
$gwTmpl = [IO.File]::ReadAllText("$PSScriptRoot\config\gateway.toml.example")
[IO.File]::WriteAllText("$kit\gateway.toml", $gwTmpl.Replace("__ROOT__", ($kit -replace "\\", "/")))

# 2. 二进制（rs-iot 三件套 + warden + reasonix）
Copy-Item "$RsiotTarget\rs-iot.exe", "$RsiotTarget\rsiot-gateway.exe", "$RsiotTarget\rsiot-mcp.exe" "$kit\bin\"
Copy-Item $WardenExe "$kit\warden\warden.exe"
Copy-Item $ReasonixExe "$kit\bin\reasonix.exe"

# 3. 设备模拟器（dist 整目录 + com0com 驱动包）
Copy-Item $SimDist "$kit\simulator\dist" -Recurse
Copy-Item $Com0com "$kit\simulator\com0com" -Recurse
# 测试场定制：控制台 8090（避让 gateway 8080）+ RTU 从站 COM3
Copy-Item "$PSScriptRoot\config\simulator-config.yaml" "$kit\simulator\dist\config.yaml" -Force

# 4. REASONIX_HOME（providers 配置 + .env 密钥；默认仅复制两个必需文件）
Copy-Item "$HomeDir\config.toml", "$HomeDir\.env" "$kit\home\"

Write-Host "[pack] $kit 组装完成："
Get-ChildItem $kit -Recurse -File | Group-Object { $_.Directory.FullName.Replace($kit, "") } |
    ForEach-Object { "{0,-40} {1,2} files" -f $_.Name, $_.Count }

Compress-Archive -Path "$kit\*" -DestinationPath "$PSScriptRoot\$OutDir.zip" -Force
Write-Host "[pack] 压缩包: $PSScriptRoot\$OutDir.zip（含 API 密钥,勿提交/外发）"
