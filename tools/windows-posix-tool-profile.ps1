$ErrorActionPreference = 'Stop'

# The declared Windows environment already supplies Git for Windows. Bind
# its companion POSIX tools to that installation before Repro resolves roles.
$gitCommand = Get-Command git.exe -CommandType Application -ErrorAction Stop
$gitExecutable = (Get-Item -LiteralPath $gitCommand.Source).FullName
$execPath = (& $gitExecutable --exec-path).Trim().Replace('/', '\')
if ($LASTEXITCODE -ne 0) { throw 'Selected Git could not report its executable directory' }
if ($execPath -notmatch '^(.*)\\mingw(?:32|64|arm64)\\libexec\\git-core$') {
    throw 'Selected Git does not have the declared Git for Windows installation layout'
}
$gitRoot = (Get-Item -LiteralPath $Matches[1]).FullName
if (-not $gitExecutable.StartsWith($gitRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Git executable and companion tool installation differ'
}
$posixBin = Join-Path $gitRoot 'usr\bin'
foreach ($name in @('dirname', 'grep', 'bash', 'sh')) {
    $tool = Join-Path $posixBin ($name + '.exe')
    if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) {
        throw "Declared Git companion tool is absent: $name"
    }
    $version = & $tool --version
    if ($LASTEXITCODE -ne 0) { throw "Declared Git companion tool refused its version check: $name" }
    Write-Host "$name SHA256=$((Get-FileHash -LiteralPath $tool -Algorithm SHA256).Hash)"
}
if (-not $env:GITHUB_PATH) { throw 'The owning CI tool-profile output is absent' }
$posixBin | Out-File -FilePath $env:GITHUB_PATH -Encoding utf8 -Append
