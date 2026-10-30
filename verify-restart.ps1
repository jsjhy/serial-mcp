# 隔离的 Windows 进程重启回归：使用系统分配端口和无效串口名，不访问真实设备。
# 覆盖真实 HTTP 回复、重复请求、旧 PID 退出、新 PID 启动与重新握手；不验证长期稳定性。
param([ValidateSet('mcp', 'both')][string]$Mode = 'mcp')
$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path -LiteralPath 'target/release/serial_mcp_tui.exe').Path
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
$testPort = $listener.LocalEndpoint.Port
$listener.Stop()
$testDir = Join-Path (Get-Location) ('target/restart-check-' + [guid]::NewGuid().ToString('N'))
$endpoint = "http://127.0.0.1:$testPort/mcp"
$headers = @{ Accept = 'application/json, text/event-stream' }

function Initialize-Session {
    $body = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"restart-check","version":"1.0"}}}'
    $response = Invoke-WebRequest -UseBasicParsing -Uri $endpoint -Method Post -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 2
    $session = [string]$response.Headers['mcp-session-id']
    if (-not $session) { throw 'No MCP session ID' }
    $headers['mcp-session-id'] = $session
    $null = Invoke-WebRequest -UseBasicParsing -Uri $endpoint -Method Post -Headers $headers -ContentType 'application/json' -Body '{"jsonrpc":"2.0","method":"notifications/initialized"}' -TimeoutSec 2
}

function Find-TestProcesses {
    Get-CimInstance Win32_Process -Filter "Name = 'serial_mcp_tui.exe'" | Where-Object {
        $_.ExecutablePath -eq $exePath -and $_.CommandLine -match "--mcp-port $testPort(?:\s|$)"
    }
}

# both 模式同时验证可见 WinAPI 窗口已创建；仅查询句柄，不模拟用户键盘/鼠标。
function Wait-GuiWindow([int]$processId) {
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $deadline) {
        $current = Get-Process -Id $processId
        if ($current.MainWindowHandle -ne 0) { return }
        Start-Sleep -Milliseconds 100
    }
    throw 'GUI window was not created'
}

try {
    # GUI 必须显式使用 Normal，否则 STARTUPINFO 的 Hidden 会覆盖首次 ShowWindow。
    $windowStyle = if ($Mode -eq 'both') { 'Normal' } else { 'Hidden' }
    $old = Start-Process -FilePath $exePath -ArgumentList @('--mode', $Mode, '--port', 'COM_RESTART_TEST_INVALID', '--mcp-port', $testPort, '--log-dir', "`"$testDir`"") -WorkingDirectory (Get-Location).Path -WindowStyle $windowStyle -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while ($true) {
        try { Initialize-Session; break } catch {
            if ([DateTime]::UtcNow -ge $deadline) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
    if ($Mode -eq 'both') { Wait-GuiWindow $old.Id }
    $body = '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"restart","arguments":{}}}'
    $first = Invoke-WebRequest -UseBasicParsing -Uri $endpoint -Method Post -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 2
    if ($first.Content -notmatch 'restart scheduled;') { throw "Unexpected first response: $($first.Content)" }
    $second = Invoke-WebRequest -UseBasicParsing -Uri $endpoint -Method Post -Headers $headers -ContentType 'application/json' -Body $body -TimeoutSec 2
    if ($second.Content -notmatch 'restart already scheduled') { throw "Unexpected repeated response: $($second.Content)" }
    if (-not $old.WaitForExit(15000)) { throw 'Old process did not exit' }
    $headers.Remove('mcp-session-id')
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while ($true) {
        try { Initialize-Session; break } catch {
            if ([DateTime]::UtcNow -ge $deadline) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
    $new = @(Find-TestProcesses | Where-Object { $_.CommandLine -notmatch '--internal-restart-helper' })
    if ($new.Count -ne 1 -or $new[0].ProcessId -eq $old.Id) { throw 'Expected exactly one replacement process with a new PID' }
    if ($Mode -eq 'both') { Wait-GuiWindow $new[0].ProcessId }
    "PASS: restart response, duplicate suppression, old PID exit, one new PID, MCP reconnection ($($old.Id) -> $($new[0].ProcessId))"
} finally {
    # 仅结束本次唯一端口对应的测试进程，不触及 dist 中正在使用的串口工具。
    Find-TestProcesses | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}
