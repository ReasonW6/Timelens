[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$DataDirectory
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\..')).Path
$dataRoot = (Resolve-Path -LiteralPath $DataDirectory).Path
$sourceFiles = @(
    Get-Item -LiteralPath (Join-Path $repoRoot 'crates\timelens-ipc\src\protocol.rs')
    Get-Item -LiteralPath (Join-Path $repoRoot 'crates\timelens-storage\src\lib.rs')
    Get-ChildItem -LiteralPath (Join-Path $repoRoot 'crates\timelens-collector\src') -Filter '*.rs' -File
    Get-ChildItem -LiteralPath (Join-Path $repoRoot 'crates\timelens-observer\src') -Filter '*.rs' -File
)
$forbiddenSourcePattern = @(
    '\b(window_title|title|url|uri|clipboard|ocr|raw_input|keystroke|character|text_content|window_text)\b',
    'GetWindowText|WM_GETTEXT|GetClipboard|OpenClipboard|GetClipboardData'
) -join '|'
$sourceHits = @($sourceFiles | Select-String -Pattern $forbiddenSourcePattern -CaseSensitive:$false)
if ($sourceHits.Count -ne 0) {
    $sourceHits | Select-Object Path, LineNumber, Line | Format-Table -AutoSize
    throw 'A forbidden content-bearing field or API was found in the collection/persistence path.'
}

$markers = @('SQLite format 3', 'Synthetic', 'capacity-desktop')
$plaintextHits = [System.Collections.Generic.List[object]]::new()
foreach ($file in Get-ChildItem -LiteralPath $dataRoot -File) {
    $text = [System.Text.Encoding]::Latin1.GetString([System.IO.File]::ReadAllBytes($file.FullName))
    foreach ($marker in $markers) {
        if ($text.Contains($marker, [System.StringComparison]::Ordinal)) {
            $plaintextHits.Add([pscustomobject]@{ File=$file.FullName; Marker=$marker })
        }
    }
}
if ($plaintextHits.Count -ne 0) {
    $plaintextHits | Format-Table -AutoSize
    throw 'Plaintext marker found in encrypted runtime data.'
}

[ordered]@{
    sourceFilesChecked = $sourceFiles.Count
    forbiddenSourceHits = 0
    encryptedFilesChecked = @(Get-ChildItem -LiteralPath $dataRoot -File).Count
    plaintextMarkers = $markers
    plaintextHits = 0
    passed = $true
} | ConvertTo-Json -Depth 3
