# Installation tasks run elevated. Every existing ancestor must prevent an
# ordinary user from renaming it or granting themselves deletion of its children.
function Assert-InstallDirectory([string] $Path) {
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
        foreach ($item in Get-ChildItem -LiteralPath $full -Force -Recurse) {
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Installation files cannot be links.' }
            $relative = $item.FullName.Substring($full.Length + 1)
            if ($relative -notmatch '^(Timelens(?:\.Collector|\.AI)?\.exe|unins\d+\.(exe|dat|msg)|internal(?:\\(register-tasks|unregister-tasks|maintenance|path-safety)\.ps1)?)$') {
                throw "The selected directory contains unrelated files. Select a dedicated Timelens folder: $relative"
            }
        }
    }
    return $full
}
