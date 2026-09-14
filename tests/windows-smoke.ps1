param([Parameter(Mandatory=$true)][string]$WmxExe)
$ErrorActionPreference = 'Stop'
$WmxExe = (Resolve-Path -LiteralPath $WmxExe).Path
$previousDirectory = $env:WMX_DIR
$env:WMX_DIR = Join-Path ([IO.Path]::GetTempPath()) ('wmx-smoke-' + [guid]::NewGuid().ToString('N'))
$name = 'smoke-session'
$clients = @()

function Run-Wmx([string[]]$Arguments, [string]$InputText = '') {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $WmxExe
    $info.Arguments = ($Arguments | ForEach-Object { '"' + $_.Replace('"', '\"') + '"' }) -join ' '
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    $stdout = $process.StandardOutput.ReadToEndAsync()
    $stderr = $process.StandardError.ReadToEndAsync()
    $process.StandardInput.Write($InputText)
    $process.StandardInput.Close()
    if (!$process.WaitForExit(15000)) { $process.Kill(); throw 'wmx timed out' }
    if ($process.ExitCode -ne 0) { throw $stderr.Result }
    return $stdout.Result
}

function Open-Client([string]$Operation, $Data) {
    $record = Get-ChildItem $env:WMX_DIR -Filter '*.json' | Select-Object -First 1
    $endpoint = [IO.File]::ReadAllText($record.FullName) | ConvertFrom-Json
    $tcp = [Net.Sockets.TcpClient]::new('127.0.0.1', $endpoint.port)
    # Windows accepts inherit nonblocking mode unless the daemon resets it.
    Start-Sleep -Milliseconds 100
    $stream = $tcp.GetStream()
    $stream.ReadTimeout = 3000
    $writer = [IO.StreamWriter]::new($stream, [Text.UTF8Encoding]::new($false))
    $writer.AutoFlush = $true
    $reader = [IO.StreamReader]::new($stream, [Text.UTF8Encoding]::new($false))
    $writer.WriteLine((@{ token=$endpoint.token; operation=$Operation; data=$Data } | ConvertTo-Json -Compress -Depth 8))
    return @{ Tcp=$tcp; Reader=$reader; Writer=$writer }
}

function Request([string]$Operation, $Data) {
    $connection = Open-Client $Operation $Data
    try {
        $reply = $connection.Reader.ReadLine() | ConvertFrom-Json
        if ($reply.error) { throw $reply.error }
        return $reply
    } finally { $connection.Tcp.Dispose() }
}

function Assert-Grid([int]$Rows, [int]$Cols, [string]$Label) {
    $grid = (Run-Wmx @('grid', $name)) | ConvertFrom-Json
    if ($grid.rows -ne $Rows -or $grid.cols -ne $Cols) { throw "$Label expected ${Rows}x${Cols}, got $($grid.rows)x$($grid.cols)" }
    Write-Output "PASS $Label"
}

try {
    $version = Run-Wmx @('version')
    if ($version -notmatch 'wire_generation\s+1') { throw 'Missing wire generation' }
    $shell = Join-Path $env:ProgramFiles 'PowerShell/7/pwsh.exe'
    if (!(Test-Path $shell)) { $shell = Join-Path $env:SystemRoot 'System32/WindowsPowerShell/v1.0/powershell.exe' }
    $launch = @{ name=$name; cwd=[IO.Path]::GetTempPath(); shell=$shell; startup="`$env:WMX_SMOKE='WMX_PERSISTENCE_OK'; Write-Output `$env:WMX_SMOKE" } | ConvertTo-Json -Compress
    $encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($launch))
    Run-Wmx @('start-encoded', $encoded) | Out-Null
    $first = Request 'ping' $null
    Run-Wmx @('start-encoded', $encoded) | Out-Null
    if ((Request 'ping' $null).shellPid -ne $first.shellPid) { throw 'Starting an existing session replaced its shell' }
    Write-Output 'PASS idempotent start'
    Assert-Grid 50 200 'headless grid'
    $a = Open-Client 'attach' @{ clientId=1; rows=30; cols=90; promptEditor='monaco' }
    $clients += $a
    $a.Reader.ReadLine() | Out-Null
    $b = Open-Client 'attach' @{ clientId=2; rows=20; cols=60; promptEditor='code-server' }
    $clients += $b
    $b.Reader.ReadLine() | Out-Null
    Assert-Grid 20 60 'latest visible client leads'
    if ((Run-Wmx @('prompt-editor-capability', $name)).Trim() -ne 'code-server') { throw 'Wrong prompt editor leader' }
    Request 'visibility' @{ clientId=2; state='parked'; rows=20; cols=60 } | Out-Null
    Assert-Grid 30 90 'hidden client yields to visible client'
    if ((Run-Wmx @('prompt-editor-capability', $name)).Trim() -ne 'monaco') { throw 'Prompt editor leadership did not change' }
    Request 'resize' @{ clientId=2; rows=10; cols=40 } | Out-Null
    Assert-Grid 30 90 'hidden native resize cannot clamp visible terminal'
    Request 'visibility' @{ clientId=1; state='chat'; rows=30; cols=90 } | Out-Null
    Assert-Grid 20 200 'chat uses freshest parked rows and resting width'
    Request 'visibility' @{ clientId=1; state='parked'; rows=30; cols=90 } | Out-Null
    Assert-Grid 20 200 'unattended grid stays unchanged'
    Request 'refresh' @{ clientId=1 } | Out-Null
    if ((Run-Wmx @('refresh-if-stale', $name, '20', '200')).Trim() -ne 'refresh-if-stale skipped') { throw 'Fresh grid was resized' }
    if ((Run-Wmx @('refresh-if-stale', $name, '21', '201')).Trim() -ne 'refresh-if-stale applied') { throw 'Stale grid was not refreshed' }
    Write-Output 'PASS refresh acknowledged and stale comparison is atomic'
    $a.Tcp.Dispose(); $b.Tcp.Dispose(); $clients=@()
    Start-Sleep -Milliseconds 1200
    $grid = (Run-Wmx @('grid', $name)) | ConvertFrom-Json
    if ($grid.clients.Count -ne 0) { throw 'Disconnected clients remain registered' }
    if ((Request 'ping' $null).shellPid -ne $first.shellPid) { throw 'Detach replaced the shell' }
    Write-Output 'PASS disconnect releases clients and preserves shell'
    $deadline = (Get-Date).AddSeconds(10)
    do {
        $history = Run-Wmx @('history', $name)
        if ($history -match 'WMX_PERSISTENCE_OK') { break }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    if ($history -notmatch 'WMX_PERSISTENCE_OK') { throw 'History lost startup output' }
    Run-Wmx @('send', $name) "Write-Output ('WMX_SEND_' + 'OK')`r" | Out-Null
    $deadline = (Get-Date).AddSeconds(10)
    do {
        $history = Run-Wmx @('history', $name)
        if ($history -match 'WMX_SEND_OK') { break }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    if ($history -notmatch 'WMX_SEND_OK') { throw 'Raw input was not delivered' }
    Write-Output 'PASS raw input and history'
    Run-Wmx @('attach', $name) | Out-Null
    Run-Wmx @('exists', $name) | Out-Null
    Write-Output 'PASS CLI attach detaches on stdin EOF without killing shell'
    Run-Wmx @('kill', $name) | Out-Null
    if ((Run-Wmx @('list', '--short')).Trim()) { throw 'Killed session still listed' }
    Run-Wmx @('start-encoded', $encoded) | Out-Null
    if ((Request 'ping' $null).shellPid -eq $first.shellPid) { throw 'Immediate restart did not create a new shell' }
    $owner = Run-Wmx @('inspect', $name) | ConvertFrom-Json
    if (!$owner.exists) { throw 'Registry liveness lost running daemon' }
    Run-Wmx @('kill', '--force', $name) | Out-Null
    $owner = Run-Wmx @('inspect', $name) | ConvertFrom-Json
    if ($owner.exists) { throw 'Forced recovery left daemon alive' }
    Write-Output 'PASS verified force recovery'
    Write-Output 'PASS kill waits for cleanup and immediate restart succeeds'
} finally {
    foreach ($connection in $clients) { $connection.Tcp.Dispose() }
    try { Run-Wmx @('kill', $name) | Out-Null } catch {}
    Remove-Item $env:WMX_DIR -Recurse -Force -ErrorAction SilentlyContinue
    $env:WMX_DIR = $previousDirectory
}
