# Installs naspeed on Windows from the GitHub release and runs `naspeed setup`.
#
#   irm https://dacsang97.github.io/naspeed/install.ps1 | iex
#
# Optional environment variables:
#   NASPEED_VERSION  release tag to install (default: the latest release)
#   NASPEED_STORE    store dir, on the drive that holds your projects (e.g. D:\.naspeed)
#
# One script block, so the console runs it as a single statement and its
# variables stay out of the caller's session.
& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell 5.1 may default to TLS 1.0, which GitHub refuses.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    # The progress bar makes Invoke-WebRequest very slow in Windows PowerShell.
    $ProgressPreference = 'SilentlyContinue'

    $repo = 'dacsang97/naspeed'
    $version = $env:NASPEED_VERSION
    if (-not $version) {
        $version = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
    }

    $arch = 'x86_64'
    try {
        if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq 'Arm64') { $arch = 'aarch64' }
    } catch {
        if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { $arch = 'aarch64' }
    }

    $name = "naspeed-$version-$arch-pc-windows-msvc"
    $url = "https://github.com/$repo/releases/download/$version/$name.zip"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("naspeed-install-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Write-Host "Downloading $name.zip"
        $zip = Join-Path $tmp "$name.zip"
        Invoke-WebRequest $url -OutFile $zip -UseBasicParsing
        Invoke-WebRequest "$url.sha256" -OutFile "$zip.sha256" -UseBasicParsing

        $expected = (Get-Content "$zip.sha256" -Raw).Trim().Split(' ')[0].ToLower()
        $actual = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) { throw "Checksum mismatch for $name.zip (expected $expected, got $actual)" }

        Expand-Archive $zip -DestinationPath $tmp
        $exe = Join-Path $tmp "$name\naspeed.exe"

        # setup copies naspeed.exe and ns.exe into %USERPROFILE%\.cargo\bin,
        # so the temporary folder can go afterwards.
        $setupArgs = @('setup')
        if ($env:NASPEED_STORE) { $setupArgs += @('--store', $env:NASPEED_STORE) }
        & $exe @setupArgs
        if ($LASTEXITCODE -ne 0) { throw "naspeed setup failed (exit code $LASTEXITCODE)" }
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}
