# SPDX-License-Identifier: GPL-3.0-or-later
#
# Loads the registered ASIO driver the way a host does and reports what came back,
# without involving Studio One. Same call `asiolist.cpp:227` makes:
#
#   CoCreateInstance(clsid, 0, CLSCTX_INPROC_SERVER, riid = the same clsid, &p)
#
# Run it after every rebuild: it catches "the DLL is listed but cannot be
# instantiated" and "the vtable is off by one" long before a DAW gets a chance to
# crash on them. A crash here is contained in this PowerShell process.
#
#   pwsh -File scripts/probe-asio-com.ps1
#   pwsh -File scripts/probe-asio-com.ps1 -Clsid '{...}'
#
# It does NOT need elevation: reading the COM registration is enough.
[CmdletBinding()]
param(
    [string]$Clsid = '{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}'
)

$ErrorActionPreference = 'Stop'

$source = @'
using System;
using System.Runtime.InteropServices;

public static class AsioComProbe {
    [DllImport("ole32.dll", ExactSpelling = true, PreserveSig = true)]
    public static extern int CoCreateInstance(ref Guid rclsid, IntPtr pUnkOuter,
        uint dwClsContext, ref Guid riid, out IntPtr ppv);

    // IASIO vtable: IUnknown is slots 0..2, then the 21 methods in the order
    // common/iasiodrv.h declares them. init=3, getDriverName=4, getDriverVersion=5,
    // getErrorMessage=6, start=7, stop=8, getChannels=9, getLatencies=10,
    // getBufferSize=11, canSampleRate=12, ...
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    public delegate int InitDelegate(IntPtr self, IntPtr sysHandle);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    public delegate int GetChannelsDelegate(IntPtr self, out int ins, out int outs);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    public delegate int GetBufferSizeDelegate(IntPtr self, out int min, out int max, out int preferred, out int granularity);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    public delegate uint ReleaseDelegate(IntPtr self);

    public static IntPtr Slot(IntPtr p, int index) {
        IntPtr vtable = Marshal.ReadIntPtr(p);
        return Marshal.ReadIntPtr(vtable, index * IntPtr.Size);
    }
    // PowerShell cannot spell an explicit generic argument the way C# can
    // ([AsioComProbe]::Fn[[AsioComProbe+InitDelegate]](..) is a parser error), so
    // there is one concrete accessor per slot instead of a generic Fn<T>.
    public static InitDelegate Init(IntPtr p) {
        return Marshal.GetDelegateForFunctionPointer<InitDelegate>(Slot(p, 3));
    }
    public static GetChannelsDelegate GetChannelsFor(IntPtr p) {
        return Marshal.GetDelegateForFunctionPointer<GetChannelsDelegate>(Slot(p, 9));
    }
    public static GetBufferSizeDelegate GetBufferSizeFor(IntPtr p) {
        return Marshal.GetDelegateForFunctionPointer<GetBufferSizeDelegate>(Slot(p, 11));
    }
    public static ReleaseDelegate ReleaseFor(IntPtr p) {
        return Marshal.GetDelegateForFunctionPointer<ReleaseDelegate>(Slot(p, 2));
    }
}
'@
Add-Type -TypeDefinition $source -Language CSharp

$clsidGuid = [Guid]$Clsid
$unknown   = [IntPtr]::Zero
# 1 = CLSCTX_INPROC_SERVER
$hr = [AsioComProbe]::CoCreateInstance([ref]$clsidGuid, [IntPtr]::Zero, 1, [ref]$clsidGuid, [ref]$unknown)
Write-Host ("CLSID            : {0}" -f $Clsid)
Write-Host ("CoCreateInstance : 0x{0:X8} {1}" -f $hr, $(if ($hr -eq 0) { '(S_OK)' } else { '(FAILED)' }))

if ($hr -ne 0 -or $unknown -eq [IntPtr]::Zero) {
    Write-Host ''
    Write-Host 'The driver could not be instantiated. Check, in this order:'
    Write-Host '  * HKLM\SOFTWARE\ASIO\<name> -> CLSID'
    Write-Host '  * HKLM\SOFTWARE\Classes\CLSID\{...}\InprocServer32 -> (default) = the DLL path'
    Write-Host '  * scripts/register-asio.ps1 with no arguments lists what is missing.'
    exit 1
}

$init = [AsioComProbe]::Init($unknown)
$initResult = $init.Invoke($unknown, [IntPtr]::Zero)
Write-Host ("init             : {0} {1}" -f $initResult, $(if ($initResult -eq 1) { '(ASIOTrue)' } else { '(ASIOFalse)' }))

$channels = [AsioComProbe]::GetChannelsFor($unknown)
$ins = 0; $outs = 0
$rc = $channels.Invoke($unknown, [ref]$ins, [ref]$outs)
Write-Host ("getChannels      : rc={0} in={1} out={2}" -f $rc, $ins, $outs)

$bufferSize = [AsioComProbe]::GetBufferSizeFor($unknown)
$min = 0; $max = 0; $preferred = 0; $granularity = 0
$rc = $bufferSize.Invoke($unknown, [ref]$min, [ref]$max, [ref]$preferred, [ref]$granularity)
Write-Host ("getBufferSize    : rc={0} min={1} max={2} preferred={3} granularity={4}" -f $rc, $min, $max, $preferred, $granularity)

$release = [AsioComProbe]::ReleaseFor($unknown)
Write-Host ("Release          : refcount {0}" -f $release.Invoke($unknown))
