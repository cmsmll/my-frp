#Requires -Version 7.0
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$SkipBuild,
    [switch]$VerifyOnly,
    [ValidateRange(1024, 65535)][int]$ServerAdminPort = 17400,
    [ValidateRange(1024, 65535)][int]$ClientAdminPort = 17401,
    [ValidateRange(1024, 65535)][int]$TransportPort = 17443,
    [ValidateRange(1024, 65535)][int]$PublicPort = 18080,
    [ValidateRange(1024, 65535)][int]$LocalPort = 18090
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$profileName = if ($Release) { 'release' } else { 'debug' }
$suffix = if ($IsWindows) { '.exe' } else { '' }
$binaryDir = Join-Path $projectRoot "target/$profileName"
$password = 'stream-demo-password-2026'
$processes = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()
$runId = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
$runDir = Join-Path $projectRoot "target/stream-demo/$runId"

function Start-DemoProcess {
    param([string]$Name, [string]$Binary, [string[]]$Arguments)
    # File paths cannot contain quotes on Windows; quote each argument for spaces.
    $options = @{
        FilePath = $Binary
        WorkingDirectory = $projectRoot
        PassThru = $true
        RedirectStandardOutput = Join-Path $runDir "$Name.stdout.log"
        RedirectStandardError = Join-Path $runDir "$Name.stderr.log"
    }
    if ($Arguments.Count -gt 0) {
        $options.ArgumentList = @($Arguments | ForEach-Object { '"' + $_ + '"' })
    }
    if ($IsWindows) { $options.WindowStyle = 'Hidden' }
    $process = Start-Process @options
    $processes.Add($process)
    return $process
}

function Wait-DemoHttp {
    param([string]$Url, [System.Diagnostics.Process]$Process)
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($Process.HasExited) { throw "Process exited before $Url became ready. Logs: $runDir" }
        try {
            $null = Invoke-RestMethod -Uri $Url -NoProxy -TimeoutSec 1
            return
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    throw "Timed out waiting for $Url. Logs: $runDir"
}

function Invoke-DemoApi {
    param([string]$Base, [string]$Path, [string]$Token, [hashtable]$Body)
    $options = @{ Uri = "$Base$Path"; NoProxy = $true; TimeoutSec = 10 }
    if ($Token) { $options.Headers = @{ Authorization = "Bearer $Token" } }
    if ($null -ne $Body) {
        $options.Method = 'Post'
        $options.ContentType = 'application/json; charset=utf-8'
        $options.Body = [System.Text.Encoding]::UTF8.GetBytes(($Body | ConvertTo-Json -Depth 5))
    }
    return Invoke-RestMethod @options
}

function Test-DemoStreams {
    param([string]$Base)
    $http = [System.Net.Http.HttpClient]::new([System.Net.Http.HttpClientHandler]@{ UseProxy = $false })
    $cancel = [System.Threading.CancellationTokenSource]::new(15000)
    $socket = [System.Net.WebSockets.ClientWebSocket]::new()
    $response = $null
    $reader = $null
    try {
        $socket.Options.Proxy = $null
        $wsUrl = $Base.Replace('http://', 'ws://') + '/ws'
        $null = $socket.ConnectAsync([uri]$wsUrl, $cancel.Token).GetAwaiter().GetResult()
        $message = 'my-frp WS / SSE demo: 你好'
        $payload = [System.Text.Encoding]::UTF8.GetBytes($message)
        $null = $socket.SendAsync([ArraySegment[byte]]::new($payload), [System.Net.WebSockets.WebSocketMessageType]::Text, $true, $cancel.Token).GetAwaiter().GetResult()
        $buffer = [byte[]]::new(4096)
        $received = [System.IO.MemoryStream]::new()
        try {
            do {
                $part = $socket.ReceiveAsync([ArraySegment[byte]]::new($buffer), $cancel.Token).GetAwaiter().GetResult()
                if ($part.MessageType -ne [System.Net.WebSockets.WebSocketMessageType]::Text) { throw 'Unexpected WebSocket message type' }
                $received.Write($buffer, 0, $part.Count)
            } until ($part.EndOfMessage)
            if ([System.Text.Encoding]::UTF8.GetString($received.ToArray()) -cne $message) { throw 'WebSocket echo did not match' }
        } finally { $received.Dispose() }

        $response = $http.GetAsync("$Base/sse?count=3&interval_ms=200&tag=script", [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead, $cancel.Token).GetAwaiter().GetResult()
        $null = $response.EnsureSuccessStatusCode()
        if ($response.Content.Headers.ContentType.MediaType -ne 'text/event-stream') { throw 'Unexpected SSE content type' }
        $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $reader = [System.IO.StreamReader]::new($stream)
        $events = [System.Collections.Generic.List[object]]::new()
        $timer = [System.Diagnostics.Stopwatch]::StartNew()
        $firstEventMs = $null
        while ($null -ne ($line = $reader.ReadLineAsync().WaitAsync($cancel.Token).GetAwaiter().GetResult())) {
            if ($line.StartsWith('data:')) {
                $event = $line.Substring(5).TrimStart() | ConvertFrom-Json
                $events.Add($event)
                if ($null -eq $firstEventMs) { $firstEventMs = $timer.ElapsedMilliseconds }
            }
        }
        if ($events.Count -ne 3) { throw "Expected 3 SSE events, received $($events.Count)" }
        for ($index = 0; $index -lt 3; $index++) {
            if ($events[$index].seq -ne ($index + 1) -or $events[$index].tag -cne 'script') { throw 'SSE event order or tag did not match' }
        }
        if ($timer.ElapsedMilliseconds - $firstEventMs -lt 150) { throw 'SSE events arrived buffered together' }
        $null = $socket.CloseAsync([System.Net.WebSockets.WebSocketCloseStatus]::NormalClosure, 'demo complete', $cancel.Token).GetAwaiter().GetResult()
        Write-Host 'PASS: real tunnel WebSocket Unicode echo and incremental SSE events.' -ForegroundColor Green
    } finally {
        if ($null -ne $reader) { $reader.Dispose() }
        if ($null -ne $response) { $response.Dispose() }
        $socket.Dispose()
        $cancel.Dispose()
        $http.Dispose()
    }
}

try {
    $ports = @($ServerAdminPort, $ClientAdminPort, $TransportPort, $PublicPort, $LocalPort)
    if (@($ports | Select-Object -Unique).Count -ne $ports.Count) { throw 'Each demo port must be different.' }
    foreach ($port in $ports) {
        $probe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, $port)
        try { $probe.Start() } finally { $probe.Stop() }
    }
    if (-not $SkipBuild) {
        Push-Location $projectRoot
        try {
            $buildArgs = @('build', '--locked', '--bins', '--example', 'stream_service')
            if ($Release) { $buildArgs += '--release' }
            & cargo @buildArgs
            if ($LASTEXITCODE -ne 0) { throw 'Cargo build failed.' }
        } finally { Pop-Location }
    }
    foreach ($binary in @("myfrps$suffix", "myfrpc$suffix", "examples/stream_service$suffix")) {
        if (-not (Test-Path -LiteralPath (Join-Path $binaryDir $binary))) { throw "Missing $binary. Run again without -SkipBuild." }
    }
    $null = New-Item -ItemType Directory -Path $runDir -Force
    $serverConfig = Join-Path $runDir 'server.json'
    @{
        data_dir = 'server'
        admin_listen = "127.0.0.1:$ServerAdminPort"
        tunnel_listen = "127.0.0.1:$TransportPort"
        public_url = "ws://127.0.0.1:$TransportPort"
        admin_password = $password
        allow_insecure = $true
        port_min = $PublicPort
        port_max = $PublicPort
    } | ConvertTo-Json | Set-Content -LiteralPath $serverConfig -Encoding utf8

    $demo = Start-DemoProcess 'local-service' (Join-Path $binaryDir "examples/stream_service$suffix") @("127.0.0.1:$LocalPort")
    $server = Start-DemoProcess 'server' (Join-Path $binaryDir "myfrps$suffix") @($serverConfig)
    $previousPassword = [Environment]::GetEnvironmentVariable('MYFRP_ADMIN_PASSWORD', 'Process')
    try {
        [Environment]::SetEnvironmentVariable('MYFRP_ADMIN_PASSWORD', $password, 'Process')
        $client = Start-DemoProcess 'client' (Join-Path $binaryDir "myfrpc$suffix") @('--data-dir', (Join-Path $runDir 'client'), '--admin-listen', "127.0.0.1:$ClientAdminPort", '--allow-insecure')
    } finally {
        [Environment]::SetEnvironmentVariable('MYFRP_ADMIN_PASSWORD', $previousPassword, 'Process')
    }

    $serverBase = "http://127.0.0.1:$ServerAdminPort"
    $clientBase = "http://127.0.0.1:$ClientAdminPort"
    $publicBase = "http://127.0.0.1:$PublicPort"
    Wait-DemoHttp "http://127.0.0.1:$LocalPort/health" $demo
    Wait-DemoHttp "$serverBase/api/meta" $server
    Wait-DemoHttp "$clientBase/api/meta" $client
    $serverAuth = Invoke-DemoApi $serverBase '/api/login' '' @{ password = $password }
    $clientAuth = Invoke-DemoApi $clientBase '/api/login' '' @{ password = $password }
    $tunnel = Invoke-DemoApi $serverBase '/api/tunnels' $serverAuth.token @{
        name = 'WS + SSE local demo'; bind_ip = '127.0.0.1'; port = $PublicPort
        enabled = $true; max_connections = 64; ttl_days = 1
    }
    $null = Invoke-DemoApi $clientBase '/api/tunnels' $clientAuth.token @{
        name = 'WS + SSE local service'; local_addr = "127.0.0.1:$LocalPort"; token = $tunnel.token; enabled = $true
    }
    $online = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        $state = Invoke-DemoApi $serverBase '/api/tunnels' $serverAuth.token $null
        if ($state.items | Where-Object { $_.config.id -eq $tunnel.id -and $_.online }) { $online = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $online) { throw "Tunnel did not come online. Logs: $runDir" }
    $health = Invoke-RestMethod -Uri "$publicBase/health" -NoProxy -TimeoutSec 5
    if ($health.service -ne 'my-frp-stream-demo') { throw 'Forwarded health check failed.' }
    Test-DemoStreams $publicBase

    Write-Host "Demo page:    $publicBase"
    Write-Host "Server UI:    $serverBase"
    Write-Host "Client UI:    $clientBase"
    Write-Host "Local service: http://127.0.0.1:$LocalPort"
    Write-Host "Config / logs: $runDir"
    Write-Host 'Demo Web password: stream-demo-password-2026'
    if ($VerifyOnly) { return }
    Write-Host 'Press Ctrl+C to stop the three demo processes.'
    while ($true) {
        foreach ($process in $processes) {
            if ($process.HasExited) { throw "Demo process exited. Logs: $runDir" }
        }
        Start-Sleep -Milliseconds 500
    }
} finally {
    foreach ($process in $processes) {
        if (-not $process.HasExited) {
            try { $process.Kill(); $process.WaitForExit() } catch { Write-Warning $_ }
        }
        $process.Dispose()
    }
}
