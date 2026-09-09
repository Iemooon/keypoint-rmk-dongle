param([string]$Hex = "rmk-central.hex")
$base = 0; $max = 0; $min = [int64]::MaxValue; $entry = ''
foreach ($l in Get-Content $Hex) {
  $t = $l.Substring(7, 2)
  if ($t -eq '02') { $base = ([Convert]::ToInt32($l.Substring(9, 4), 16) -shl 4) }
  elseif ($t -eq '04') { $base = ([Convert]::ToInt32($l.Substring(9, 4), 16) -shl 16) }
  elseif ($t -eq '05') { $entry = $l.Substring(9, 8) }
  elseif ($t -eq '00') {
    $a = $base + [Convert]::ToInt32($l.Substring(3, 4), 16)
    if ($a -gt $max) { $max = $a }
    if ($a -lt $min) { $min = $a }
    $end = $a + [Convert]::ToInt32($l.Substring(1, 2), 16)
    if ($end -gt $max) { $max = $end }
  }
}
"{0}: data 0x{1:X} .. 0x{2:X}  entry 0x{3}" -f $Hex, $min, $max, $entry
if ($max -le 0xA0000) { "OK: app ends below storage start 0xA0000" } else { "OVERFLOW into storage!" }
