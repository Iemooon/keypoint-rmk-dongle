$ErrorActionPreference = 'Stop'
Set-Location C:\Users\lemon\keypoint-rmk-dongle
Start-Process -FilePath gh -ArgumentList 'auth','refresh','-s','workflow','-h','github.com' `
  -RedirectStandardOutput ghrefresh.out -RedirectStandardError ghrefresh.err `
  -WindowStyle Hidden
Start-Sleep -Seconds 8
Write-Output '---OUT---'; Get-Content ghrefresh.out -ErrorAction SilentlyContinue
Write-Output '---ERR---'; Get-Content ghrefresh.err -ErrorAction SilentlyContinue
