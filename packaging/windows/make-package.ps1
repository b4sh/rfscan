<#
.SYNOPSIS
  Builds the Windows package of rfscan: a zip with rfscan.exe, SoapySDR, the driver
  modules for Airspy, RTL-SDR and SDRplay with their libraries, the Visual C++ runtime, the
  licenses and the list of sources; and a separate zip with the source code of the GPL and LGPL
  libraries in it.

.DESCRIPTION
  Layout of the package (flat: rfscan points SoapySDR to its own directory when
  lib\SoapySDR\modules0.8 exists next to rfscan.exe):

    rfscan-<version>-win64\
      rfscan.exe, SoapySDR.dll, airspy.dll, rtlsdr.dll, libusb-1.0.dll, pthreadVC2.dll,
      msvcp140.dll, vcruntime140.dll, vcruntime140_1.dll, README.txt, LICENSE
      lib\SoapySDR\modules0.8\airspySupport.dll, rtlsdrSupport.dll
      extras\sdrplay\sdrPlaySupport.dll  (copied to the modules by an SDRplay owner)
      licenses\  (license texts, SOURCES.txt, the Rust crates)

  Sources of the files: SoapySDR, its modules, libairspy, pthreads-win32 and the Visual C++
  runtime and libusb from the PothosSDR 2021.07.25 installation; librtlsdr from a Windows build of
  osmocom (the PothosSDR rtlsdr.dll crashes when a device is released). sdrplay_api.dll is
  not included (proprietary); the SDRplay module is in extras, so that without sdrplay_api.dll
  SoapySDR does not report a failed module at every start.

  Downloads (cached in dist\downloads): the osmocom build, the license header of libairspy and
  the source archives of librtlsdr, libusb and pthreads-win32.

.EXAMPLE
  pwsh packaging\windows\make-package.ps1
#>
param(
    # The PothosSDR 2021.07.25 installation.
    [string]$PothosSDR = 'C:\Program Files\PothosSDR',
    # The osmocom Windows build of rtl-sdr (date in its name) and the git commit it was built from.
    [string]$OsmocomBuild = '20261018',
    [string]$OsmocomCommit = '797f8143266d983c56d8f35d2d442527529dd8a5',
    # Skip `cargo build --release` (use the existing target\release\rfscan.exe).
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$repo = (Resolve-Path "$PSScriptRoot\..\..").Path
$version = (Select-String -Path "$repo\Cargo.toml" -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$dist = Join-Path $repo 'dist'
$downloads = Join-Path $dist 'downloads'
$name = "rfscan-$version-win64"
$pkg = Join-Path $dist $name
$srcName = "rfscan-$version-third-party-sources"
$srcDir = Join-Path $dist $srcName

# Exact versions of the components (PothosSDR 2021.07.25 built the master branches of that day).
$commits = @{
    SoapySDR      = '1cf5a539a21414ff509ff7d0eedfc5fa8edb90c6'
    SoapyAirspy   = '411f73e16532cf6c0fe40209b83e7115ad43c108'
    SoapyRTLSDR   = '53ee8f4a19dd97011a569cfc2e6f7ad63f896100'
    SoapySDRPlay3 = '206b241df0b741c5746bdc9f4347a3a3e7852d51'
    libairspy     = '022870fd88c026c124683fa58531a3a70181ebe9'
    pthreads      = '2fe8aba6a8a4ce09f353f34881c77f93a9c01ca3'
}
$libusbVersion = '1.0.24'

function Download([string]$url, [string]$file) {
    $path = Join-Path $downloads $file
    if (-not (Test-Path $path)) {
        Write-Host "download $url"
        Invoke-WebRequest -Uri $url -OutFile $path
    }
    return $path
}

function Copy-To([string]$from, [string]$dir, [string]$as = '') {
    if (-not (Test-Path $from)) { throw "missing: $from" }
    New-Item -ItemType Directory -Force $dir | Out-Null
    $target = if ($as) { Join-Path $dir $as } else { $dir }
    Copy-Item $from $target
}

# --- build -------------------------------------------------------------------------------------
if (-not $SkipBuild) {
    Push-Location $repo
    try { cargo build --release; if ($LASTEXITCODE) { throw 'cargo build failed' } } finally { Pop-Location }
}

foreach ($d in @($pkg, $srcDir)) { if (Test-Path $d) { Remove-Item -Recurse -Force $d } }
New-Item -ItemType Directory -Force $downloads, $pkg, $srcDir | Out-Null

# --- osmocom librtlsdr ------------------------------------------------------------------------
$osmoZip = Download "https://ftp.osmocom.org/binaries/windows/rtl-sdr/rtl-sdr-64bit-$OsmocomBuild.zip" "rtl-sdr-64bit-$OsmocomBuild.zip"
$osmoDir = Join-Path $downloads "rtl-sdr-64bit-$OsmocomBuild"
if (-not (Test-Path $osmoDir)) { Expand-Archive $osmoZip -DestinationPath $downloads }

# --- program files -----------------------------------------------------------------------------
$bin = Join-Path $PothosSDR 'bin'
$modules = Join-Path $PothosSDR 'lib\SoapySDR\modules0.8'
$pkgModules = Join-Path $pkg 'lib\SoapySDR\modules0.8'
Copy-To "$repo\target\release\rfscan.exe" $pkg
# libusb 1.0.24 of PothosSDR for librtlsdr and libairspy: with the newer libusb of the osmocom build
# the PothosSDR airspy.dll crashes when rfscan exits (checked 2026-10-20).
foreach ($f in 'SoapySDR.dll', 'airspy.dll', 'libusb-1.0.dll', 'pthreadVC2.dll', 'msvcp140.dll', 'vcruntime140.dll', 'vcruntime140_1.dll') {
    Copy-To (Join-Path $bin $f) $pkg
}
# The module expects rtlsdr.dll; the osmocom build calls it librtlsdr.dll (the same C API).
Copy-To "$osmoDir\librtlsdr.dll" $pkg 'rtlsdr.dll'
foreach ($f in 'airspySupport.dll', 'rtlsdrSupport.dll') {
    Copy-To (Join-Path $modules $f) $pkgModules
}
# Without sdrplay_api.dll the module fails to load and SoapySDR prints an error at every start:
# it is installed by hand (README.txt).
Copy-To (Join-Path $modules 'sdrPlaySupport.dll') (Join-Path $pkg 'extras\sdrplay')
Copy-To "$repo\LICENSE" $pkg

# --- licenses ----------------------------------------------------------------------------------
$lic = Join-Path $pkg 'licenses'
$pl = Join-Path $PothosSDR 'licenses'
Copy-To "$pl\SoapySDR\LICENSE_1_0.txt" $lic 'SoapySDR-BSL-1.0.txt'
Copy-To "$pl\SoapyAirspy\LICENSE.txt" $lic 'SoapyAirspy-MIT.txt'
Copy-To "$pl\SoapyRTLSDR\LICENSE.txt" $lic 'SoapyRTLSDR-MIT.txt'
Copy-To "$pl\SoapySDRPlay3\LICENSE.txt" $lic 'SoapySDRPlay3-MIT.txt'
Copy-To "$pl\rtl-sdr\COPYING" $lic 'rtl-sdr-GPL-2.0.txt'
Copy-To "$pl\Pthreads\COPYING.LIB" $lic 'pthreads-win32-LGPL-2.1.txt'
Copy-To "$pl\Pthreads\COPYING" $lic 'pthreads-win32-COPYING.txt'
Copy-To "$pl\libusb\COPYING" $lic 'libusb-LGPL-2.1.txt'
# libairspy has no license file: the BSD license is the header of its sources.
$airspyC = Download "https://raw.githubusercontent.com/airspy/airspyone_host/$($commits.libairspy)/libairspy/src/airspy.c" 'airspy.c'
$header = (Get-Content $airspyC -Raw) -replace '(?s)^(/\*.*?\*/).*', '$1'
Set-Content -Path "$lic\libairspy-BSD-3-Clause.txt" -Value $header -Encoding utf8

Set-Content -Path "$lic\Microsoft-Visual-C++-runtime.txt" -Encoding utf8 -Value @'
msvcp140.dll, vcruntime140.dll and vcruntime140_1.dll are the Microsoft Visual C++ runtime
(version 14.28), redistributable files of Microsoft Visual Studio, included for the libraries
built with it (SoapySDR, its modules, libairspy, pthreads-win32) and for rfscan.exe. They are
licensed by Microsoft under the Microsoft Software License Terms of Visual Studio, which allow
distributing them with an application. https://visualstudio.microsoft.com/license-terms/
'@

# Rust crates in the program (normal dependencies for Windows), with their license files.
Push-Location $repo
try {
    $tree = cargo tree -e normal --target x86_64-pc-windows-msvc --prefix none --format '{p}|{l}'
    $meta = cargo metadata --format-version 1 | ConvertFrom-Json
} finally { Pop-Location }
$manifests = @{}
foreach ($p in $meta.packages) { $manifests["$($p.name) v$($p.version)"] = $p.manifest_path }
$crates = $tree | ForEach-Object { ($_ -replace ' \(.*?\)', '').Trim() } | Where-Object { $_ -and $_ -notmatch '^rfscan ' } | Sort-Object -Unique
$rustDir = Join-Path $lic 'rust'
New-Item -ItemType Directory -Force $rustDir | Out-Null
$list = @("Rust crates compiled into rfscan.exe (name, version, license). Their license files are in the",
          "subdirectories of this directory.", "")
foreach ($c in $crates) {
    $id, $license = $c -split '\|', 2
    $list += "$id  $license"
    $dir = Split-Path $manifests[$id]
    $files = Get-ChildItem $dir -File | Where-Object { $_.Name -match '^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE|COPYRIGHT)' }
    if ($files) {
        $target = Join-Path $rustDir ($id -replace ' v', '-')
        New-Item -ItemType Directory -Force $target | Out-Null
        $files | Copy-Item -Destination $target
    } else {
        $list[-1] += '  (no license file in the crate; see its license field)'
    }
}
Set-Content -Path "$rustDir\CRATES.txt" -Value $list -Encoding utf8

# --- list of sources ---------------------------------------------------------------------------
$sources = @"
rfscan $version for Windows: components and their sources

rfscan.exe
  rfscan $version, AGPL-3.0-or-later (LICENSE). Source: https://github.com/b4sh/rfscan
  Rust crates compiled into it: licenses\rust\CRATES.txt

SoapySDR.dll
  SoapySDR 0.8.1, BSL-1.0, from PothosSDR 2021.07.25
  https://github.com/pothosware/SoapySDR/tree/$($commits.SoapySDR)

lib\SoapySDR\modules0.8\airspySupport.dll
  SoapyAirspy, MIT, from PothosSDR 2021.07.25
  https://github.com/pothosware/SoapyAirspy/tree/$($commits.SoapyAirspy)

lib\SoapySDR\modules0.8\rtlsdrSupport.dll
  SoapyRTLSDR, MIT, from PothosSDR 2021.07.25
  https://github.com/pothosware/SoapyRTLSDR/tree/$($commits.SoapyRTLSDR)

extras\sdrplay\sdrPlaySupport.dll
  SoapySDRPlay3, MIT, from PothosSDR 2021.07.25 (needs sdrplay_api.dll of the SDRplay API,
  not included: see README.txt)
  https://github.com/pothosware/SoapySDRPlay3/tree/$($commits.SoapySDRPlay3)

airspy.dll
  libairspy (airspyone_host), BSD-3-Clause, from PothosSDR 2021.07.25
  https://github.com/airspy/airspyone_host/tree/$($commits.libairspy)

pthreadVC2.dll
  pthreads-win32 2.9, LGPL-2.1-or-later, from PothosSDR 2021.07.25 (built with the PothosSDR
  patch pthreads_win32_vc14.diff)
  https://github.com/VFR-maniac/pthreads-win32/tree/$($commits.pthreads)

rtlsdr.dll
  librtlsdr (osmocom rtl-sdr), GPL-2.0-or-later, the osmocom Windows build of $OsmocomBuild
  (librtlsdr.dll, renamed for the SoapySDR module), built from commit $OsmocomCommit
  https://ftp.osmocom.org/binaries/windows/rtl-sdr/rtl-sdr-64bit-$OsmocomBuild.zip
  https://gitea.osmocom.org/sdr/rtl-sdr/src/commit/$OsmocomCommit (mirror: https://github.com/osmocom/rtl-sdr)

libusb-1.0.dll
  libusb $libusbVersion, LGPL-2.1-or-later, from PothosSDR 2021.07.25 (also used by rtlsdr.dll)
  https://github.com/libusb/libusb/releases/tag/v$libusbVersion

msvcp140.dll, vcruntime140.dll, vcruntime140_1.dll
  Microsoft Visual C++ runtime 14.28, from PothosSDR 2021.07.25 (licenses\Microsoft-Visual-C++-runtime.txt)

The complete source code of the GPL and LGPL libraries (librtlsdr, libusb, pthreads-win32) of
exactly these versions is published with this release as $srcName.zip.
"@
Set-Content -Path "$lic\SOURCES.txt" -Value $sources -Encoding utf8

# --- README.txt --------------------------------------------------------------------------------
Set-Content -Path "$pkg\README.txt" -Encoding utf8 -Value @"
rfscan $version for Windows (64-bit)

A fast radio scanner for SDR receivers. Unpack this directory anywhere and run rfscan.exe from a
terminal (Windows Terminal recommended):

  rfscan -h                  the most used options
  rfscan --list-devices      receivers that are seen
  rfscan --scan-range 144M-146M --gain "agc=true,VGA=7"

SoapySDR and the drivers for Airspy and RTL-SDR are included; PothosSDR is not needed.
USB driver: an RTL-SDR needs the WinUSB driver (install it with Zadig, https:\\zadig.akeo.ie\),
an Airspy needs nothing.

SDRplay: install the SDRplay API (sdrplay.com), then
  - copy extras\sdrplay\sdrPlaySupport.dll to lib\SoapySDR\modules0.8\
  - copy sdrplay_api.dll (x64) of your SDRplay API installation next to rfscan.exe
  (e.g. from C:\Program Files\SDRplay\API\x64). The driver module then warns that the API version
differs from its build version, but works (checked with API 3.15).

The gain (--gain) and the settle time (--settle, rfscan --calibrate) are the basis of good
results. Documentation: https://github.com/b4sh/rfscan (README.md and DOC.md).

License: AGPL-3.0-or-later (LICENSE). The included libraries and their licenses and sources:
licenses\SOURCES.txt.
"@

# --- source archives of the GPL and LGPL libraries ----------------------------------------------
Copy-Item (Download "https://codeload.github.com/osmocom/rtl-sdr/tar.gz/$OsmocomCommit" "rtl-sdr-$OsmocomCommit.tar.gz") $srcDir
Copy-Item (Download "https://github.com/libusb/libusb/releases/download/v$libusbVersion/libusb-$libusbVersion.tar.bz2" "libusb-$libusbVersion.tar.bz2") $srcDir
Copy-Item (Download "https://codeload.github.com/VFR-maniac/pthreads-win32/tar.gz/$($commits.pthreads)" "pthreads-win32-$($commits.pthreads).tar.gz") $srcDir
Copy-To (Join-Path $PothosSDR 'patches\pthreads_win32_vc14.diff') $srcDir
Set-Content -Path "$srcDir\README.txt" -Encoding utf8 -Value @"
Source code of the GPL and LGPL libraries in the Windows package of rfscan ${version}:

  rtl-sdr-$OsmocomCommit.tar.gz
      librtlsdr (rtlsdr.dll), GPL-2.0-or-later: osmocom rtl-sdr at commit $OsmocomCommit,
      from which the osmocom Windows build $OsmocomBuild was made
  libusb-$libusbVersion.tar.bz2
      libusb-1.0.dll, LGPL-2.1-or-later
  pthreads-win32-$($commits.pthreads).tar.gz and pthreads_win32_vc14.diff
      pthreadVC2.dll, LGPL-2.1-or-later: the commit built by PothosSDR 2021.07.25 with its patch

The other libraries in the package are under permissive licenses (BSL-1.0, MIT, BSD-3-Clause);
their sources are listed in licenses\SOURCES.txt of the package.
"@

# --- zips --------------------------------------------------------------------------------------
foreach ($d in @($name, $srcName)) {
    $zip = Join-Path $dist "$d.zip"
    if (Test-Path $zip) { Remove-Item $zip }
    # Some crate license files are older than 1980: zip stores them as 1980 (a harmless warning).
    Compress-Archive -Path (Join-Path $dist $d) -DestinationPath $zip -WarningAction SilentlyContinue
    Write-Host ("{0}  {1:N1} MB" -f $zip, ((Get-Item $zip).Length / 1MB))
}
