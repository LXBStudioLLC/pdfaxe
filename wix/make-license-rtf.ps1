# Regenerates wix\License.rtf from ..\LICENSE for the installer's EULA box.
# The license text is verbatim; only the display formatting changes: hard-wrapped
# paragraphs are rejoined so the dialog can flow them, deeply indented lines
# (the centered GNU headings) are centered and bolded, and 4+ space indented
# blocks (the how-to-apply sample notice) keep their line breaks.
# Output is committed - rerun only if LICENSE ever changes.
# ASCII only: Windows PowerShell 5.1 reads BOM-less scripts as ANSI.
$ErrorActionPreference = "Stop"

$src = Join-Path $PSScriptRoot "..\LICENSE"
$dst = Join-Path $PSScriptRoot "License.rtf"

function Escape-Rtf([string]$s) {
    $s = $s.Replace('\', '\\').Replace('{', '\{').Replace('}', '\}')
    $sb = New-Object System.Text.StringBuilder
    foreach ($ch in $s.ToCharArray()) {
        $code = [int]$ch
        if ($code -gt 127) { [void]$sb.Append("\u$code?") } else { [void]$sb.Append($ch) }
    }
    $sb.ToString()
}

# Split into blank-line-separated paragraphs.
$paragraphs = New-Object System.Collections.ArrayList
$current = New-Object System.Collections.ArrayList
foreach ($line in (Get-Content $src)) {
    if ($line.Trim().Length -eq 0) {
        if ($current.Count -gt 0) { [void]$paragraphs.Add($current.ToArray()); $current.Clear() }
    } else {
        [void]$current.Add($line)
    }
}
if ($current.Count -gt 0) { [void]$paragraphs.Add($current.ToArray()) }

$rtf = New-Object System.Text.StringBuilder
[void]$rtf.Append('{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fnil\fcharset0 Segoe UI;}}')
[void]$rtf.AppendLine()
[void]$rtf.Append('\f0\fs16 ')
[void]$rtf.AppendLine()

foreach ($para in $paragraphs) {
    $allDeep = @($para | Where-Object { $_ -notmatch '^\s{10,}' }).Count -eq 0
    $allIndented = @($para | Where-Object { $_ -notmatch '^\s{4,}' }).Count -eq 0
    if ($allDeep) {
        # Centered heading block (title, "Preamble", "TERMS AND CONDITIONS", ...).
        foreach ($line in $para) {
            [void]$rtf.Append('\pard\qc\sa60\b ')
            [void]$rtf.Append((Escape-Rtf $line.Trim()))
            [void]$rtf.Append('\b0\par')
            [void]$rtf.AppendLine()
        }
    } elseif ($allIndented) {
        # Preformatted block: keep the line structure.
        [void]$rtf.Append('\pard\ql\li240\sa120 ')
        for ($i = 0; $i -lt $para.Count; $i++) {
            [void]$rtf.Append((Escape-Rtf $para[$i].Trim()))
            if ($i -lt $para.Count - 1) { [void]$rtf.Append('\line ') }
        }
        [void]$rtf.Append('\par')
        [void]$rtf.AppendLine()
    } else {
        # Body paragraph: rejoin the hard-wrapped lines and let the box flow them.
        $joined = (($para | ForEach-Object { $_.Trim() }) -join ' ')
        [void]$rtf.Append('\pard\ql\sa120 ')
        [void]$rtf.Append((Escape-Rtf $joined))
        [void]$rtf.Append('\par')
        [void]$rtf.AppendLine()
    }
}
[void]$rtf.Append('}')

# ASCII, no BOM - the RTF header declares its own codepage.
[System.IO.File]::WriteAllText($dst, $rtf.ToString(), [System.Text.Encoding]::ASCII)
Write-Host "Wrote $dst ($((Get-Item $dst).Length) bytes, $($paragraphs.Count) paragraphs)"
