# box_local_convert.ps1 — convert ONE already-on-box raw to .mzpeak, NO S3.
# Used by the direct-SCP path (tools/box_convert_scp.sh): the raw is scp'd to the box, converted
# here, and the .mzpeak scp'd back. Sets the same vendor SDK env as box_convert_remote.ps1.
#   powershell -NoProfile -File box_local_convert.ps1 -InPath <box raw> -OutPath <box out.mzpeak>
param([Parameter(Mandatory=$true)][string]$InPath, [Parameter(Mandatory=$true)][string]$OutPath)
$ErrorActionPreference = 'Continue'
$ProgressPreference    = 'SilentlyContinue'
$env:DOTNET_ROOT = 'C:\Users\User\dotnet8'; $env:DOTNET_ROLL_FORWARD = 'LatestMajor'
$cvtRoot = 'C:\Users\User\src\mzPeakConverter'
$pwiz = 'C:/Program Files (x86)/FLASHApp-1.0.0/FLASHApp-1.0.0/share/OpenMS/THIRDPARTY/pwiz-bin'
if (Test-Path "$cvtRoot\glue\sciex\bin\Release\net8.0")  { $env:MZPC_SCIEX_GLUE  = (Resolve-Path "$cvtRoot\glue\sciex\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\waters\bin\Release\net8.0") { $env:MZPC_WATERS_GLUE = (Resolve-Path "$cvtRoot\glue\waters\bin\Release\net8.0").Path }
if (Test-Path "$cvtRoot\glue\agilent\bin\Release\net48") { $env:MZPC_AGILENT_GLUE = (Resolve-Path "$cvtRoot\glue\agilent\bin\Release\net48").Path }
$env:MZPC_PWIZ_DIR = $pwiz; $env:MZPC_MASSLYNX_DIR = $pwiz
if (Test-Path 'C:\Users\User\box_convert_env.ps1') { . 'C:\Users\User\box_convert_env.ps1' }
$conv = "$cvtRoot\target\release\mzpeak-convert.exe"
$log = "$OutPath.log"   # redirect target must be a path token/variable, NOT a parenthesized expr
& $conv $InPath --no-vendor -o $OutPath --force *> $log
"EXIT=$LASTEXITCODE"
if (Test-Path $OutPath) { "SIZE=" + (Get-Item $OutPath).Length } else { "NO_OUTPUT" }
