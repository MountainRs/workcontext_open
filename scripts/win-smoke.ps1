<#
.SYNOPSIS
  WorkContext Windows 原生冒烟：干净 runner 上用 tauri-driver/WebDriver 驱动 release 应用，
  真实切三页、真实点击纠错，证明冷启动、重启与纠错落库持久化。

.DESCRIPTION
  六个步骤（任一步失败即停止并以非零码退出）：
    cold              tauri-driver 拉起 release WorkContext；WebDriver 真实点击侧边栏切三页
                      （今日 /today → 全部事项 /items → 迭代池 /pool），逐页断言数据经 IPC 加载；
                      空库下今日 0 条、列表 0 行；应用在沙箱建库。
    seed              只预置 A11「纠错前」状态（scripts/win-smoke/seed_a11_pre_correction.sql：
                      一条 active/me 的合成事项 + 证据 + created 事件；合成来源 enabled=0、未授权）。
                      同时断言预置数据的范围隔离：库与数据目录都在沙箱内，库里只有这些合成行。
    correct           再次拉起应用：今日「轮到我」恰好 1 条、列表恰好 1 行；进入详情页，填写备注并
                      点击「不是我的事」；断言人工锁 belongs_to_me + lifecycle、生命周期 ignored、
                      纠错历史，且点一次即移出今日「轮到我」与列表「轮到我」筛选（A11）。
    verify-corrected  逐行核对应用自己写出的纠错：两把人工锁、一条 corrected(human) 事件、事项
                      lifecycle=ignored 且 attention=no_next；写入时刻晚于预置（确由点击产生）；
                      其余业务表原样、无同步/出域记录。
    restart           再次冷启动：三页可切；今日为 0；从列表进入详情，人工锁与纠错留痕仍在。
    verify-restart    业务表与纠错后快照逐行完全一致（跨重启持久化，未被静默恢复）。

  隔离（绝不动日常用户数据库）：
    * tauri 的 app_data_dir = %APPDATA%\com.local.workcontext 经 Known Folder 解析，进程环境变量
      改不掉。本脚本在启动应用前，用目录 junction 把它整体指进一次性沙箱；该目录已存在
      （日常数据）则直接拒绝运行，不做备份/恢复式的触碰。
    * WebView2 用户数据目录用 WEBVIEW2_USER_DATA_FOLDER 重定向进沙箱（Tauri 2.12 未覆盖该变量，
      wry 未显式指定 data_directory 时 loader 以它为准）。
    * 全程不点同步/授权/登录；合成来源停用且未授权，同步引擎不会触碰它（不访问企业内容）。
    * teardown：先确认应用退出，再只摘除 junction 本身（rmdir 不带 /s），最后删除沙箱。
      junction 摘不掉时保留沙箱并以码 3 退出，并打印手动清理命令。

  退出码：0 通过；1 某个步骤的断言失败；2 预检/隔离/自举失败；3 teardown 风险（junction 未摘除）。
  产物（-OutDir，默认 target\win-smoke-artifacts）：smoke-report.json、各步 report-*.json、
  baseline-*.json、逐页/逐状态截图 shot-*.png、win-smoke.log。

  Runner 依赖（GitHub 托管的 windows-latest 全部自带或可联网获取）：
    * Windows x64 + 可显示 GUI 的交互式会话（应用窗口与 msedgedriver 需要；Session 0 服务不行）
    * PowerShell 7（pwsh）；不保证 Windows PowerShell 5.1
    * Node.js >= 22（锁定的 selenium-webdriver 4.51 的要求；scripts/win-smoke/package.json）
    * Python >= 3.8，且自带 SQLite >= 3.34 并启用 FTS5（python.org 的 Windows 构建自带；
      迁移用了 fts5 trigram 虚表与触发器；预检会实测）
    * Rust stable + cargo（首次运行 cargo install tauri-driver --locked）
    * Microsoft Edge WebView2 Runtime（通常预装；缺失时联网装 Evergreen bootstrapper）
    * 网络：npm registry、https://msedgedriver.microsoft.com（下载与 WebView2 运行时同版本的驱动）
    * 已构建的 release 可执行文件（frontendDist 在编译期嵌入 ui/dist）：
        pnpm install --frozen-lockfile; pnpm build
        pnpm exec tauri build --bundles nsis      # 或 cargo build --release -p workcontext-desktop

.EXAMPLE
  # CI（GitHub Actions，放在 "Build Windows installer" 之后；失败也上传产物以便定位）
  #   - name: Windows native smoke (release + tauri-driver)
  #     if: runner.os == 'Windows'
  #     run: pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/win-smoke.ps1
  #   - uses: actions/upload-artifact@v4
  #     if: runner.os == 'Windows' && always()
  #     with:
  #       name: win-smoke-artifacts
  #       path: target/win-smoke-artifacts
  #       if-no-files-found: warn

.EXAMPLE
  # 本地复现（干净的用户 profile：该账号从未运行过 WorkContext）
  pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/win-smoke.ps1 -KeepSandbox

.EXAMPLE
  # 离线/预置工具：驱动与 tauri-driver 已就位时跳过所有自举
  pwsh -NoProfile -File scripts/win-smoke.ps1 -SkipBootstrap `
    -MsEdgeDriverPath C:\tools\msedgedriver.exe -TauriDriverPath "$env:USERPROFILE\.cargo\bin\tauri-driver.exe"
#>

param(
  # release 可执行文件；默认 target\release\workcontext-desktop.exe
  [string]$ExePath = "",
  # 产物目录；默认 target\win-smoke-artifacts（也可用环境变量 WORKCONTEXT_SMOKE_OUT）
  [string]$OutDir = "",
  # 纠错时在详情页填写的备注（A11 口径），同时作为落库核对的期望值
  [string]$CorrectionNote = '已口头改由李四接手推进，消息里没有体现',
  # 跳过所有自举（WebView2 / tauri-driver / msedgedriver / npm ci 均须已就绪）
  [switch]$SkipBootstrap,
  # 保留沙箱与 junction 供调试（结束后手动：cmd /c rmdir "<AppDataDir>"，不要加 /s）
  [switch]$KeepSandbox,
  # 使用预先准备好的 msedgedriver.exe（须与 WebView2 运行时同版本），跳过下载
  [string]$MsEdgeDriverPath = "",
  # 使用指定的 tauri-driver 可执行文件，跳过 cargo install
  [string]$TauriDriverPath = "",
  [int]$Port = 4444,
  [int]$NativePort = 4445
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

# app_data_dir = %APPDATA%\<identifier>（crates/desktop/tauri.conf.json 的 identifier）
$AppIdentifier = 'com.local.workcontext'
# 数据库文件名（crates/core/src/lib.rs 的 DB_FILENAME）
$DbFilename = 'workcontext.sqlite3'
$AppProcessName = 'workcontext-desktop'
# WebView2 Runtime 在 EdgeUpdate 客户端表里的 Key（机器级 HKLM\...\WOW6432Node 与 HKLM、用户级 HKCU 各查一次）
$WebView2ClientKey = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$Phases = @('cold', 'seed', 'correct', 'verify-corrected', 'restart', 'verify-restart')

$RepoRoot = Split-Path -Parent $PSScriptRoot        # scripts/ 的上一级 = 仓库根
$Support = Join-Path $PSScriptRoot 'win-smoke'

$script:ExitCode = 0
$script:FailureKind = $null     # preflight | phase | teardown
$Sandbox = $null
$SandboxBin = $null
$AppDataDir = $null
$JunctionCreated = $false
$PrevWebview2UserData = $env:WEBVIEW2_USER_DATA_FOLDER

function Write-Log([string]$Message) { Write-Host "[win-smoke] $Message" }

function Fail([string]$Kind, [string]$Message) {
  $script:FailureKind = $Kind
  throw $Message
}

function Invoke-Tool {
  # 原生命令的 stderr（如 cargo 的编译进度）不是 PowerShell 错误，退出码才是判据；
  # 因此调用期间放宽 ErrorActionPreference，结束后按退出码归类失败。
  param(
    [string]$Label, [string]$FilePath, [string[]]$ToolArguments,
    [string]$WorkingDirectory = '', [string]$Kind = 'phase'
  )
  Write-Log "运行: $FilePath $($ToolArguments -join ' ')"
  $previous = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  $code = -1
  try {
    if ($WorkingDirectory) {
      Push-Location $WorkingDirectory
      try { & $FilePath @ToolArguments; $code = $LASTEXITCODE }
      finally { Pop-Location }
    } else {
      & $FilePath @ToolArguments
      $code = $LASTEXITCODE
    }
  } finally { $ErrorActionPreference = $previous }
  if ($code -ne 0) { Fail $Kind "$Label 失败（退出码 $code）" }
}

function Test-WebView2Runtime {
  foreach ($key in @(
      "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$WebView2ClientKey",
      "HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$WebView2ClientKey",
      "HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\$WebView2ClientKey")) {
    if (Test-Path $key) {
      $pv = (Get-ItemProperty -Path $key -ErrorAction SilentlyContinue).pv
      if ($pv -and $pv -ne '0.0.0.0') { return $pv }
    }
  }
  return $null
}

function Wait-AppExit([int]$TimeoutSec = 30) {
  $deadline = (Get-Date).AddSeconds($TimeoutSec)
  while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process -Name $AppProcessName -ErrorAction SilentlyContinue)) { return $true }
    Start-Sleep -Milliseconds 500
  }
  $left = Get-Process -Name $AppProcessName -ErrorAction SilentlyContinue
  if ($left) {
    $left | Stop-Process -Force
    Write-Log "警告：$AppProcessName 未在 ${TimeoutSec}s 内退出，已强制结束（WAL 保证已提交数据完整）"
    return $false
  }
  return $true
}

try {
  # ---------- 0. 预检 ----------
  if ([Environment]::OSVersion.Platform -ne 'Win32NT') { Fail 'preflight' '本脚本只在 Windows 上运行' }
  Write-Log "仓库根: $RepoRoot"

  if (-not $ExePath) { $ExePath = Join-Path $RepoRoot "target\release\$AppProcessName.exe" }
  if (-not (Test-Path -LiteralPath $ExePath)) {
    Fail 'preflight' (@(
        "找不到 release 可执行文件: $ExePath",
        '请先构建（frontendDist 在编译期嵌入 ui/dist）:',
        '  pnpm install --frozen-lockfile; pnpm build',
        '  pnpm exec tauri build --bundles nsis   （或 cargo build --release -p workcontext-desktop）') -join [Environment]::NewLine)
  }
  $ExePath = (Resolve-Path -LiteralPath $ExePath).Path

  if (-not $OutDir) { $OutDir = $env:WORKCONTEXT_SMOKE_OUT }
  if (-not $OutDir) { $OutDir = Join-Path $RepoRoot 'target\win-smoke-artifacts' }
  New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
  $OutDir = (Resolve-Path -LiteralPath $OutDir).Path
  try { Start-Transcript -Path (Join-Path $OutDir 'win-smoke.log') | Out-Null } catch { }

  $Node = Get-Command node -ErrorAction SilentlyContinue
  if (-not $Node) { Fail 'preflight' '缺少 node（需要 Node.js >= 22：锁定的 selenium-webdriver 4.51 的要求）' }
  $NodeVersion = (& $Node.Source --version).Trim()
  if ([int]($NodeVersion.TrimStart('v').Split('.')[0]) -lt 22) {
    Fail 'preflight' "Node.js 版本过低（$NodeVersion），需要 >= 22"
  }
  $PythonExe = $null
  $PythonPrefix = @()
  if (Get-Command python -ErrorAction SilentlyContinue) { $PythonExe = (Get-Command python).Source }
  elseif (Get-Command python3 -ErrorAction SilentlyContinue) { $PythonExe = (Get-Command python3).Source }
  elseif (Get-Command py -ErrorAction SilentlyContinue) { $PythonExe = (Get-Command py).Source; $PythonPrefix = @('-3') }
  if (-not $PythonExe) { Fail 'preflight' '缺少 Python 3（需要 >= 3.8，标准库 sqlite3）' }
  # 迁移用了 FTS5 trigram 分词的虚表与触发器：预置写入与落库核对要求 Python 自带的 SQLite
  # 带该能力（python.org 的 Windows 构建自带），否则在预检就给出明确原因。
  Invoke-Tool 'Python 版本/sqlite3 自检' $PythonExe ($PythonPrefix + @(
      '-c', 'import sqlite3, sys; assert sys.version_info >= (3, 8), sys.version; assert sqlite3.sqlite_version_info >= (3, 34, 0), sqlite3.sqlite_version; sqlite3.connect(":memory:").execute("CREATE VIRTUAL TABLE probe USING fts5(x, tokenize=''trigram'')")'
    )) '' 'preflight'

  # WebView2 Runtime（应用 GUI 的宿主）；版本同时决定要下载哪个 msedgedriver
  $WebView2Version = Test-WebView2Runtime
  if (-not $WebView2Version) {
    if ($SkipBootstrap) { Fail 'preflight' '未检测到 WebView2 Runtime 且指定了 -SkipBootstrap' }
    $Sandbox = Join-Path ([IO.Path]::GetTempPath()) ("workcontext-smoke-" + (Get-Date -Format 'yyyyMMdd-HHmmss') + "-$PID")
    New-Item -ItemType Directory -Force -Path (Join-Path $Sandbox 'setup') | Out-Null
    $Bootstrapper = Join-Path $Sandbox 'setup\MicrosoftEdgeWebview2Setup.exe'
    Write-Log '未检测到 WebView2 Runtime：下载并静默安装 Evergreen bootstrapper...'
    Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $Bootstrapper -UseBasicParsing
    Invoke-Tool 'WebView2 Runtime 安装' $Bootstrapper @('/silent', '/install') '' 'preflight'
    Start-Sleep -Seconds 3
    $WebView2Version = Test-WebView2Runtime
    if (-not $WebView2Version) { Fail 'preflight' 'WebView2 安装后仍未检测到版本' }
  }
  Write-Log "WebView2 Runtime: $WebView2Version"

  # ---------- 1. 沙箱与隔离 ----------
  if (-not $Sandbox) {
    $Sandbox = Join-Path ([IO.Path]::GetTempPath()) ("workcontext-smoke-" + (Get-Date -Format 'yyyyMMdd-HHmmss') + "-$PID")
  }
  $SandboxAppData = Join-Path $Sandbox 'appdata'
  $SandboxWebView2 = Join-Path $Sandbox 'webview2'
  $SandboxBin = Join-Path $Sandbox 'bin'
  foreach ($dir in @($SandboxAppData, $SandboxWebView2, $SandboxBin)) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
  }
  $Sandbox = (Resolve-Path -LiteralPath $Sandbox).Path
  $SandboxAppData = (Resolve-Path -LiteralPath $SandboxAppData).Path
  $SandboxBin = (Resolve-Path -LiteralPath $SandboxBin).Path

  $Roaming = [Environment]::GetFolderPath('ApplicationData')
  $AppDataDir = Join-Path $Roaming $AppIdentifier
  if (Test-Path -LiteralPath $AppDataDir) {
    Fail 'preflight' (@(
        "拒绝运行：$AppDataDir 已存在。",
        '冒烟绝不动日常用户数据库。请使用干净的 CI runner；',
        '本地复跑需换一个从未运行过 WorkContext 的 Windows 账号（profile）。') -join [Environment]::NewLine)
  }
  New-Item -ItemType Junction -Path $AppDataDir -Target $SandboxAppData | Out-Null
  $link = Get-Item -LiteralPath $AppDataDir -Force
  if ($link.LinkType -ne 'Junction') {
    Fail 'preflight' "junction 校验失败（LinkType=$($link.LinkType)），未启动应用"
  }
  $JunctionCreated = $true
  Write-Log "隔离: $AppDataDir  ──junction──>  $SandboxAppData"
  $DbPath = Join-Path $SandboxAppData $DbFilename

  # WebView2 用户数据目录重定向（loader 级环境变量，子进程继承）
  $env:WEBVIEW2_USER_DATA_FOLDER = Join-Path $Sandbox 'webview2'

  # ---------- 2. WebDriver 工具链 ----------
  # 2.1 tauri-driver：WebDriver 中介（--port 对客户端，--native-port 对 msedgedriver）
  $TauriDriver = $TauriDriverPath
  if (-not $TauriDriver) {
    $cmd = Get-Command tauri-driver -ErrorAction SilentlyContinue
    if ($cmd) { $TauriDriver = $cmd.Source }
    elseif (Test-Path -LiteralPath (Join-Path $env:USERPROFILE '.cargo\bin\tauri-driver.exe')) {
      $TauriDriver = Join-Path $env:USERPROFILE '.cargo\bin\tauri-driver.exe'
    }
  }
  if (-not $TauriDriver) {
    if ($SkipBootstrap) { Fail 'preflight' '未找到 tauri-driver 且指定了 -SkipBootstrap（可先 cargo install tauri-driver --locked）' }
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) { Fail 'preflight' '缺少 cargo，无法安装 tauri-driver' }
    Write-Log '安装 tauri-driver（cargo install tauri-driver --locked）...'
    Invoke-Tool 'cargo install tauri-driver' $cargo.Source @('install', 'tauri-driver', '--locked') '' 'preflight'
    $TauriDriver = Join-Path $env:USERPROFILE '.cargo\bin\tauri-driver.exe'
  }
  if (-not (Test-Path -LiteralPath $TauriDriver)) { Fail 'preflight' "tauri-driver 不存在: $TauriDriver" }
  Write-Log "tauri-driver: $TauriDriver"

  # 2.2 msedgedriver：必须与 WebView2 运行时同版本（版本不匹配会卡在连接）
  $MsEdgeDriver = $MsEdgeDriverPath
  if ($MsEdgeDriver -and -not (Test-Path -LiteralPath $MsEdgeDriver)) {
    Fail 'preflight' "-MsEdgeDriverPath 不存在: $MsEdgeDriver"
  }
  if (-not $MsEdgeDriver) {
    $arch = switch ($env:PROCESSOR_ARCHITECTURE) {
      'ARM64' { 'arm64' }
      'x86' { 'win32' }
      default { 'win64' }
    }
    $zip = Join-Path $SandboxBin "edgedriver_$arch.zip"
    # Microsoft Edge WebDriver 下载页当前的主机；azureedge 是旧主机，留作回退。
    foreach ($driverHost in @('msedgedriver.microsoft.com', 'msedgedriver.azureedge.net')) {
      $driverUrl = "https://$driverHost/$WebView2Version/edgedriver_$arch.zip"
      try {
        Write-Log "下载 msedgedriver $WebView2Version ($arch): $driverUrl"
        Invoke-WebRequest -Uri $driverUrl -OutFile $zip -UseBasicParsing
        break
      } catch {
        Write-Log "下载失败: $($_.Exception.Message)"
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
      }
    }
    if (Test-Path -LiteralPath $zip) {
      Expand-Archive -Path $zip -DestinationPath $SandboxBin -Force
      $found = Get-ChildItem -Path $SandboxBin -Recurse -Filter 'msedgedriver.exe' | Select-Object -First 1
      if ($found) { $MsEdgeDriver = $found.FullName }
    }
  }
  if (-not $MsEdgeDriver) {
    # 回退：官方文档提到的 msedgedriver-tool（自行匹配 WebView2 版本并下载）
    if ($SkipBootstrap) { Fail 'preflight' "msedgedriver 获取失败（WebView2 $WebView2Version）且指定了 -SkipBootstrap" }
    $cargo = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargo) { Fail 'preflight' 'msedgedriver 下载失败，且缺少 cargo 无法回退 msedgedriver-tool' }
    Write-Log '回退：cargo install --git https://github.com/chippers/msedgedriver-tool'
    Invoke-Tool 'cargo install msedgedriver-tool' $cargo.Source @('install', '--git', 'https://github.com/chippers/msedgedriver-tool') '' 'preflight'
    $tool = Join-Path $env:USERPROFILE '.cargo\bin\msedgedriver-tool.exe'
    Invoke-Tool 'msedgedriver-tool' $tool @() $SandboxBin 'preflight'
    $found = Get-ChildItem -Path @($SandboxBin, (Split-Path -Parent $tool)) -Recurse -Filter 'msedgedriver.exe' -ErrorAction SilentlyContinue |
      Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($found) { $MsEdgeDriver = $found.FullName }
  }
  if (-not (Test-Path -LiteralPath $MsEdgeDriver)) { Fail 'preflight' 'msedgedriver 获取失败（直连与 msedgedriver-tool 均未得到 msedgedriver.exe）' }
  Write-Log "msedgedriver: $MsEdgeDriver"

  # 2.3 WebDriver 客户端依赖（scripts/win-smoke 独立的 npm 项目，不进应用依赖）
  if (-not (Test-Path -LiteralPath (Join-Path $Support 'node_modules\selenium-webdriver'))) {
    if ($SkipBootstrap) { Fail 'preflight' "缺少 selenium-webdriver（$Support\node_modules）且指定了 -SkipBootstrap" }
    $npm = Get-Command npm -ErrorAction SilentlyContinue
    if (-not $npm) { Fail 'preflight' '缺少 npm，无法安装 selenium-webdriver' }
    Write-Log '安装 WebDriver 客户端依赖（npm ci）...'
    Invoke-Tool 'npm ci' $npm.Source @('ci', '--omit=dev', '--no-fund', '--no-audit') $Support 'preflight'
  }

  # ---------- 3~8. 六个步骤 ----------
  $SmokeArgs = @(
    (Join-Path $Support 'smoke.mjs'),
    '--exe', $ExePath, '--tauri-driver', $TauriDriver, '--native-driver', $MsEdgeDriver,
    '--db', $DbPath, '--out', $OutDir,
    '--port', "$Port", '--native-port', "$NativePort",
    '--note', $CorrectionNote
  )
  $VerifyArgs = @(
    '--db', $DbPath, '--app-data', $AppDataDir, '--sandbox', $Sandbox,
    '--migrations', (Join-Path $RepoRoot 'migrations')
  )
  $SeedVerify = Join-Path $Support 'seed_verify.py'
  $BaselinePre = Join-Path $OutDir 'baseline-pre.json'
  $BaselinePost = Join-Path $OutDir 'baseline-post.json'

  Write-Log '=== cold：冷启动 + 切三页 ==='
  Invoke-Tool 'cold' $Node.Source ($SmokeArgs + @('--phase', 'cold')) $Support
  Wait-AppExit | Out-Null

  Write-Log '=== seed：预置 A11 纠错前状态 + 范围隔离断言 ==='
  Invoke-Tool 'seed' $PythonExe ($PythonPrefix + @($SeedVerify, 'seed') + $VerifyArgs + @(
      '--report', (Join-Path $OutDir 'report-seed.json'), '--baseline', $BaselinePre))

  Write-Log '=== correct：真实点击「不是我的事」 ==='
  Invoke-Tool 'correct' $Node.Source ($SmokeArgs + @('--phase', 'correct')) $Support
  Wait-AppExit | Out-Null

  Write-Log '=== verify-corrected：核对应用写出的纠错 ==='
  Invoke-Tool 'verify-corrected' $PythonExe ($PythonPrefix + @($SeedVerify, 'verify-corrected') + $VerifyArgs + @(
      '--report', (Join-Path $OutDir 'report-verify-corrected.json'),
      '--baseline', $BaselinePre, '--baseline-out', $BaselinePost, '--note', $CorrectionNote))

  Write-Log '=== restart：冷启动重启 + 切三页 + 详情复验 ==='
  Invoke-Tool 'restart' $Node.Source ($SmokeArgs + @('--phase', 'restart')) $Support
  Wait-AppExit | Out-Null

  Write-Log '=== verify-restart：跨重启逐行比对 ==='
  Invoke-Tool 'verify-restart' $PythonExe ($PythonPrefix + @($SeedVerify, 'verify-restart') + $VerifyArgs + @(
      '--report', (Join-Path $OutDir 'report-verify-restart.json'), '--baseline', $BaselinePost))

  # ---------- 9. 汇总 ----------
  $reports = [ordered]@{}
  foreach ($name in $Phases) {
    $file = Join-Path $OutDir "report-$name.json"
    $reports[$name] = if (Test-Path -LiteralPath $file) {
      Get-Content -LiteralPath $file -Raw -Encoding UTF8 | ConvertFrom-Json
    } else { $null }
  }
  $failed = @($reports.Keys | Where-Object { -not $reports[$_] -or -not $reports[$_].ok })
  $summary = [ordered]@{
    ok             = ($failed.Count -eq 0)
    finishedAt     = (Get-Date).ToUniversalTime().ToString('o')
    exe            = $ExePath
    correctionNote = $CorrectionNote
    appDataDir     = $AppDataDir
    db             = $DbPath
    webView2       = $WebView2Version
    tauriDriver    = $TauriDriver
    msedgedriver   = $MsEdgeDriver
    failedPhases   = $failed
    phases         = $reports
  }
  $summary | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $OutDir 'smoke-report.json') -Encoding UTF8
  if ($failed.Count -gt 0) { Fail 'phase' "冒烟失败: $($failed -join ', ')（详见 $OutDir\smoke-report.json）" }
  Write-Log "冒烟通过。报告: $(Join-Path $OutDir 'smoke-report.json')"
}
catch {
  Write-Log "失败[$script:FailureKind]: $($_.Exception.Message)"
  if (-not $script:FailureKind) { $script:FailureKind = 'preflight' }
  $script:ExitCode = if ($script:FailureKind -eq 'phase') { 1 } else { 2 }
}
finally {
  # ---------- teardown：先确认进程退出，再只摘 junction，最后删沙箱 ----------
  try { Wait-AppExit 15 | Out-Null } catch { }
  if ($SandboxBin) {
    Get-Process -Name msedgedriver -ErrorAction SilentlyContinue |
      Where-Object { $_.Path -and $_.Path.StartsWith($SandboxBin, [StringComparison]::OrdinalIgnoreCase) } |
      Stop-Process -Force -ErrorAction SilentlyContinue
  }
  if ($JunctionCreated) {
    $item = Get-Item -LiteralPath $AppDataDir -Force -ErrorAction SilentlyContinue
    if ($item -and $item.LinkType -eq 'Junction') {
      # rmdir（不带 /s）只摘除 junction 本身，不动沙箱内容
      try { & cmd.exe /c rmdir "$AppDataDir" } catch { Write-Log "摘除 junction 异常: $($_.Exception.Message)" }
    }
    if (Test-Path -LiteralPath $AppDataDir) {
      Write-Log "严重：junction 仍在（$AppDataDir）。请手动执行: cmd /c rmdir `"$AppDataDir`"（不要加 /s）"
      Write-Log "在此之前不要启动 WorkContext，否则数据会写进沙箱。沙箱已保留: $Sandbox"
      $script:ExitCode = 3
      $KeepSandbox = $true
    } else {
      Write-Log "junction 已摘除: $AppDataDir"
    }
  }
  if ($Sandbox) {
    if ($KeepSandbox) { Write-Log "保留沙箱（调试）: $Sandbox" }
    else {
      Remove-Item -LiteralPath $Sandbox -Recurse -Force -ErrorAction SilentlyContinue
      Write-Log "沙箱已删除: $Sandbox"
    }
  }
  if ($null -ne $PrevWebview2UserData) { $env:WEBVIEW2_USER_DATA_FOLDER = $PrevWebview2UserData }
  else { Remove-Item Env:\WEBVIEW2_USER_DATA_FOLDER -ErrorAction SilentlyContinue }
  try { Stop-Transcript | Out-Null } catch { }
}

exit $script:ExitCode
