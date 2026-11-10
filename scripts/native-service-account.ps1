param(
    [Parameter(Mandatory = $true)][ValidateSet('create', 'remove')][string]$Action,
    [Parameter(Mandatory = $true)][string]$Directory
)
$ErrorActionPreference = 'Stop'
if ($env:AK_NATIVE_SERVICE_TESTS -ne '1' -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Service account acceptance is restricted to explicitly enabled, disposable GitHub-hosted VMs.'
}
$account = 'ak-service-ci'
$marker = Join-Path $Directory 'owned-account.sid'
if ($Action -eq 'remove') {
    if (Test-Path -LiteralPath $marker) {
        $user = Get-LocalUser -Name $account -ErrorAction SilentlyContinue
        if ($user -and $user.SID.Value -eq (Get-Content -LiteralPath $marker -Raw).Trim()) {
            Remove-LocalUser -Name $account
        } elseif ($user) {
            throw 'Refusing to remove a service account with a different SID.'
        }
    }
    exit 0
}
if (Get-LocalUser -Name $account -ErrorAction SilentlyContinue) {
    throw 'Refusing to reuse an existing account.'
}
if (!$env:AK_TEST_SERVICE_PASSWORD) { throw 'Missing temporary test password.' }
$password = ConvertTo-SecureString $env:AK_TEST_SERVICE_PASSWORD -AsPlainText -Force
$user = New-LocalUser -Name $account -Password $password -AccountNeverExpires -UserMayNotChangePassword
$user.SID.Value | Set-Content -LiteralPath $marker
$users = Get-LocalGroup -SID 'S-1-5-32-545'
Add-LocalGroupMember -Group $users -Member $user

# Grant only this account SeServiceLogonRight through LSA; do not rewrite the
# runner's existing user-rights policy or grant administrator membership.
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;
public static class ArtifactServiceTestLogon {
    [StructLayout(LayoutKind.Sequential)] struct Attributes {
        public uint Length;
        public IntPtr RootDirectory, ObjectName;
        public uint Flags;
        public IntPtr SecurityDescriptor, SecurityQualityOfService;
    }
    [StructLayout(LayoutKind.Sequential)] struct UnicodeString {
        public ushort Length, MaximumLength;
        public IntPtr Buffer;
    }
    [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref Attributes attributes, uint access, out IntPtr policy);
    [DllImport("advapi32.dll")] static extern uint LsaAddAccountRights(IntPtr policy, byte[] sid, UnicodeString[] rights, uint count);
    [DllImport("advapi32.dll")] static extern uint LsaNtStatusToWinError(uint status);
    [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr policy);
    static void Check(uint status) { if (status != 0) throw new Win32Exception((int)LsaNtStatusToWinError(status)); }
    public static void Grant(string value) {
        var attributes = new Attributes();
        attributes.Length = (uint)Marshal.SizeOf(typeof(Attributes));
        IntPtr policy;
        Check(LsaOpenPolicy(IntPtr.Zero, ref attributes, 0x810, out policy));
        var right = "SeServiceLogonRight";
        var buffer = Marshal.StringToHGlobalUni(right);
        try {
            var sid = new SecurityIdentifier(value);
            var bytes = new byte[sid.BinaryLength];
            sid.GetBinaryForm(bytes, 0);
            var text = new UnicodeString { Length = (ushort)(right.Length * 2), MaximumLength = (ushort)((right.Length + 1) * 2), Buffer = buffer };
            Check(LsaAddAccountRights(policy, bytes, new[] { text }, 1));
        } finally { Marshal.FreeHGlobal(buffer); LsaClose(policy); }
    }
}
'@
[ArtifactServiceTestLogon]::Grant($user.SID.Value)
$grants = @(
    "$($env:COMPUTERNAME)\$($account):(OI)(CI)F",
    '*S-1-5-18:(OI)(CI)F',
    '*S-1-5-32-544:(OI)(CI)F',
    "$([Security.Principal.WindowsIdentity]::GetCurrent().Name):(OI)(CI)F"
)
& icacls.exe $Directory '/inheritance:r' '/grant:r' @grants '/T' '/Q'
if ($LASTEXITCODE -ne 0) { throw 'Cannot set private test configuration ACLs.' }
$allowed = @($user.SID.Value, 'S-1-5-18', 'S-1-5-32-544', [Security.Principal.WindowsIdentity]::GetCurrent().User.Value)
$paths = @((Get-Item -LiteralPath $Directory)) + @(Get-ChildItem -LiteralPath $Directory -Recurse -Force)
foreach ($path in $paths) {
    $acl = Get-Acl -LiteralPath $path.FullName
    if (!$acl.AreAccessRulesProtected) { throw "ACL inheritance still enabled: $($path.FullName)" }
    foreach ($rule in $acl.Access) {
        $sid = $rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
        if ($sid -notin $allowed) { throw "Unexpected ACL identity $sid on $($path.FullName)" }
    }
}
