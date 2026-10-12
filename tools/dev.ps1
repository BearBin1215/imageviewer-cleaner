#!/usr/bin/env pwsh
# 开发模式：改代码 -> 自动重建 -> 自动重启窗口。
# 改动 crates/ 或 data/ 后自动重建并重启，不需要额外安装 cargo-watch。
#
# 用法：
#   pwsh -File tools/dev.ps1              # 构建并运行界面
#   pwsh -File tools/dev.ps1 -Scan        # 只跑探测逻辑，结果写 scan-output.txt
#
# 几点说明：
# - 采用轮询：编辑器保存常见「写临时文件再改名替换」，
#   FileSystemWatcher 会漏掉这类事件，轮询写入时间戳不受影响。
# - 构建前先结束旧进程：Windows 会锁定正在运行的 exe，不结束就链接失败。
# - 固定带 dev-no-elevation：发布清单要求管理员，每次重启都弹 UAC 会把流程卡死。

[CmdletBinding()]
param(
    # 以 --scan 启动：不弹界面，验证探测逻辑
    [switch]$Scan,
    # 轮询间隔（毫秒）
    [int]$IntervalMs = 300,
    # 监听范围：目录按后缀过滤，文件直接取时间戳
    [string[]]$Paths = @(
        'crates',
        'data',
        'Cargo.toml',
        'Cargo.lock',
        'rust-toolchain.toml'
    )
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Root = Split-Path -Parent $PSScriptRoot
# 归一化成反斜杠绝对路径，便于和进程的实际路径比对
$Exe = [System.IO.Path]::GetFullPath((Join-Path $Root 'target\debug\imageviewer-cleaner.exe'))
$WatchExtensions = @('.rs', '.toml', '.json', '.ttf', '.rc', '.manifest')
$script:Last = [datetime]::MinValue
$script:Child = $null

# 被监听文件里最新的写入时间，作为「有没有变动」的判据
function Get-NewestWrite {
    $best = [datetime]::MinValue
    foreach ($p in $Paths) {
        $full = Join-Path $Root $p
        if (-not (Test-Path -LiteralPath $full)) { continue }

        $files = if ((Get-Item -LiteralPath $full).PSIsContainer) {
            Get-ChildItem -LiteralPath $full -File -Recurse |
                Where-Object { $WatchExtensions -contains $_.Extension }
        } else {
            @(Get-Item -LiteralPath $full)
        }

        foreach ($f in $files) {
            if ($f.LastWriteTimeUtc -gt $best) { $best = $f.LastWriteTimeUtc }
        }
    }
    $best
}

# 阻塞直到有文件变动
function Wait-Change {
    while ($true) {
        Start-Sleep -Milliseconds $IntervalMs
        if ((Get-NewestWrite) -ne $script:Last) {
            # 一次保存可能分几次落盘，稍等后再取一次，别拿半份文件去构建
            Start-Sleep -Milliseconds 400
            $script:Last = Get-NewestWrite
            return
        }
    }
}

# 只挑本脚本负责的那份 debug 产物：release 版同名但路径不同，不能误杀
function Get-OurProcess {
    $found = @()
    foreach ($p in Get-Process -Name 'imageviewer-cleaner' -ErrorAction SilentlyContinue) {
        # 个别进程读 Path 会抛异常，取不到就当不是我们的
        try {
            if ($p.Path -ieq $Exe) { $found += $p }
        } catch { }
    }
    $found
}

function Stop-Child {
    # 除了自己启动的那个，也顺带清扫遗留的同产物实例（上次被强杀后残留的进程，
    # 或 F5 调试起的实例）。它们会锁定 exe，让紧接着的链接步骤失败。
    foreach ($p in Get-OurProcess) {
        Write-Host ('结束进程 {0}' -f $p.Id) -ForegroundColor DarkGray
        Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    }
    $script:Child = $null
}

function Start-Child {
    $spec = @{
        FilePath         = $Exe
        WorkingDirectory = $Root
        NoNewWindow      = $true
        PassThru         = $true
    }
    if ($Scan) { $spec['ArgumentList'] = @('--scan') }
    $script:Child = Start-Process @spec
}

function Invoke-Build {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    & cargo build -p imageviewer-cleaner-gui --features dev-no-elevation
    if ($LASTEXITCODE -eq 0) {
        Write-Host ('构建完成 {0:N1}s' -f $sw.Elapsed.TotalSeconds) -ForegroundColor Green
        $true
    } else {
        Write-Host ('构建失败 {0:N1}s，保存任意文件即可重试' -f $sw.Elapsed.TotalSeconds) -ForegroundColor Red
        $false
    }
}

# Ctrl+C 时 finally 未必按预期跑，再挂一个 trap 兜底，避免残留进程
trap {
    Stop-Child
    Write-Host '开发模式已退出' -ForegroundColor DarkGray
    break
}

$script:Last = Get-NewestWrite
Write-Host ("监听：{0}`n启动首次构建，Ctrl+C 退出" -f ($Paths -join '、')) -ForegroundColor Cyan

try {
    while ($true) {
        Stop-Child

        if (Invoke-Build) {
            Start-Child
            # 自检模式跑完就退出，否则它会立刻结束并让下一轮误判为「没启动」
            if ($Scan) { $script:Child.WaitForExit() }
        }

        # 把构建自身造成的变动（例如 cargo 重写 Cargo.lock）排除在外
        $script:Last = Get-NewestWrite
        Wait-Change
        Write-Host '检测到文件变动' -ForegroundColor Cyan
    }
} finally {
    Stop-Child
    Write-Host '开发模式已退出' -ForegroundColor DarkGray
}
