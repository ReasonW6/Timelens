# The collector and maintenance scripts run elevated, so every existing ancestor of
# their directory must prevent an ordinary user from renaming it or granting
# themselves deletion of its children. The user-chosen directory only holds
# ordinary-privilege programs and may live under any fixed-disk folder.
function Assert-InstallDirectory([string] $Path, [switch] $RequireProtectedAncestors) {
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $root = [IO.Path]::GetPathRoot($full)
    if ($full.Length -le $root.Length -or ([IO.DriveInfo]::new($root)).DriveType -ne [IO.DriveType]::Fixed) {
        throw 'Select a dedicated application directory on a fixed local disk.'
    }
    $trusted = @('S-1-5-18', 'S-1-5-32-544', 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464')
    $parent = Split-Path -Parent $full
    while ($parent) {
        if (Test-Path -LiteralPath $parent) {
            $item = Get-Item -LiteralPath $parent -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Installation paths cannot traverse reparse points.' }
        }
        if ($RequireProtectedAncestors -and (Test-Path -LiteralPath $parent)) {
            $acl = Get-Acl -LiteralPath $parent
            $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
            if ($owner -notin $trusted) { throw "The parent directory is owned by an ordinary user and can be replaced: $parent. Choose Program Files or an administrator-protected folder." }
            foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
                if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
                    ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) -or
                    $rule.IdentityReference.Value -in $trusted) { continue }
                # DELETE, WRITE_DAC, WRITE_OWNER, FILE_DELETE_CHILD.
                if ([long]$rule.FileSystemRights -band 0xD0040) {
                    throw "An ordinary principal can replace the installation parent: $parent. Choose an administrator-protected folder."
                }
            }
        }
        $parent = Split-Path -Parent $parent
    }
    if (Test-Path -LiteralPath $full) {
        if ((Get-Item -LiteralPath $full -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'The installation directory cannot be a link.' }
        # The ordinary-privilege directory may hold the user's dataset in its Data
        # folder. Only that folder itself is checked; its contents belong to the user.
        $children = @(Get-ChildItem -LiteralPath $full -Force)
        $data = $children | Where-Object { $_.Name -ieq 'Data' -and -not $RequireProtectedAncestors }
        if ($data -and (-not $data.PSIsContainer -or ($data.Attributes -band [IO.FileAttributes]::ReparsePoint))) {
            throw 'The Data folder in the installation directory must be a plain directory.'
        }
        $children = @($children | Where-Object { $_ -ne $data })
        $nested = @($children | Where-Object { $_.PSIsContainer -and -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) } | ForEach-Object { Get-ChildItem -LiteralPath $_.FullName -Force -Recurse })
        foreach ($item in $children + $nested) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Installation files cannot be links.' }
            $relative = $item.FullName.Substring($full.Length + 1)
            if ($relative -notmatch '^(Timelens(?:\.Collector|\.AI)?\.exe|unins\d+\.(exe|dat|msg)|internal(?:\\(register-tasks|unregister-tasks|maintenance|path-safety)\.ps1)?)$') {
                throw "The selected directory contains unrelated files. Select a dedicated Timelens folder: $relative"
            }
        }
    }
    return $full
}
