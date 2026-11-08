# 流界 FlowAcross — Windows 开发环境一键配置
#
# 用法（在项目根目录打开 PowerShell）：
#   powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
#
# 会按需安装：Visual Studio 2022 生成工具（C++）、WebView2 运行时、Rust、Node.js LTS，
# 然后安装前端依赖。已经装好的会自动跳过。需要 Windows 10 1809+ 或 Windows 11（自带 winget）。

$ErrorActionPreference = "Stop"

function Step($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Ok($msg) { Write-Host "    $msg" -ForegroundColor Green }
function Has($cmd) { [bool](Get-Command $cmd -ErrorAction SilentlyContinue) }
function Refresh-Path {
    $env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" +
                [Environment]::GetEnvironmentVariable("Path", "User") + ";" +
                "$env:USERPROFILE\.cargo\bin"
}
function Winget-Install($id, $extra = @()) {
    winget install --id $id -e --source winget --accept-package-agreements --accept-source-agreements @extra
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne -1978335189) {  # -1978335189 = 已安装
        throw "安装 $id 失败（winget 退出码 $LASTEXITCODE）"
    }
}

Step "检查 winget"
if (-not (Has winget)) {
    throw "未找到 winget。请先在 Microsoft Store 安装或更新「应用安装程序」(App Installer)，然后重新运行本脚本。"
}
Ok "winget 可用"

Step "Visual Studio 2022 生成工具（C++ 桌面开发）"
# Rust 的 MSVC 工具链需要微软的 C++ 链接器 link.exe 和 Windows SDK。
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsSetup = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\setup.exe"
$vcComponent = "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
$sdkComponent = "Microsoft.VisualStudio.Component.Windows11SDK.22621"

function Find-VC {
    if (-not (Test-Path $vswhere)) { return $null }
    & $vswhere -products * -latest -requires $vcComponent -property installationPath
}

function Find-LinkExe($vsPath) {
    Get-ChildItem -Path (Join-Path $vsPath "VC\Tools\MSVC") -Filter link.exe -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "Hostx64\\x64" } | Select-Object -First 1
}

$vcPath = Find-VC
if ($vcPath) {
    Ok "已安装：$vcPath"
} else {
    $existing = $null
    if (Test-Path $vswhere) { $existing = & $vswhere -products * -latest -property installationPath }
    if ($existing) {
        # 已有 Visual Studio / Build Tools 但没装 C++：winget 会认为"已安装"而跳过，必须用安装器的 modify 补装组件。
        Write-Host "    检测到已安装的 Visual Studio：$existing"
        Write-Host "    但缺少 C++ 生成工具，正在补装（会弹出管理员授权和安装进度窗口）…"
        $modifyArgs = "modify --installPath `"$existing`" --add $vcComponent --add $sdkComponent --passive --norestart --wait"
        $proc = Start-Process -FilePath $vsSetup -ArgumentList $modifyArgs -Verb RunAs -Wait -PassThru
        if ($proc.ExitCode -eq 3010) { Write-Host "    安装完成，系统提示需要重启后才能完全生效。" -ForegroundColor Yellow }
    } else {
        Write-Host "    正在安装（约 2–6 GB，需要几分钟到几十分钟，会显示安装进度窗口）…"
        Winget-Install "Microsoft.VisualStudio.2022.BuildTools" @(
            "--override", "--passive --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
        )
    }
    $vcPath = Find-VC
    if (-not $vcPath) {
        throw ("C++ 生成工具仍未安装成功。请在开始菜单打开「Visual Studio Installer」，" +
               "在 Build Tools 2022（或你的 Visual Studio）上点「修改」，勾选「使用 C++ 的桌面开发」后安装，然后重新运行本脚本。")
    }
    Ok "已安装：$vcPath"
}
$link = Find-LinkExe $vcPath
if ($link) { Ok "找到链接器：$($link.FullName)" } else {
    throw "Visual Studio 中找不到 link.exe。请在「Visual Studio Installer」里确认已勾选「MSVC 生成工具（x64/x86）」和「Windows 11 SDK」。"
}

Step "WebView2 运行时"
$wv2 = "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"
if ((Test-Path $wv2) -or (Test-Path "HKCU:\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}")) {
    Ok "已安装（Windows 11 自带）"
} else {
    Winget-Install "Microsoft.EdgeWebView2Runtime"
    Ok "安装完成"
}

Step "Rust 工具链"
Refresh-Path
if (-not (Has rustup)) {
    Winget-Install "Rustlang.Rustup"
    Refresh-Path
}
rustup default stable-msvc | Out-Null
rustup update stable | Out-Null
Ok (rustc --version)

Step "Node.js LTS"
if (-not (Has node)) {
    Winget-Install "OpenJS.NodeJS.LTS"
    Refresh-Path
}
Ok ("node " + (node --version))

Step "安装前端依赖"
Push-Location (Join-Path $PSScriptRoot "..\app")
try {
    npm install
    if ($LASTEXITCODE -ne 0) { throw "npm install 失败" }
} finally {
    Pop-Location
}
Ok "完成"

Write-Host "`n全部就绪。" -ForegroundColor Green
Write-Host "请关闭并重新打开 PowerShell（让新的 PATH 生效），然后运行："
Write-Host "    cd app"
Write-Host "    npm run tauri dev"
Write-Host "首次运行时 Windows 防火墙会询问是否允许流界访问网络，请勾选「专用网络」并点「允许访问」。"
