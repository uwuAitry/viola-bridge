# SPDX-License-Identifier: GPL-3.0-or-later
#
# Loads the registered ASIO driver the way a host does and walks the whole
# initialisation sequence, reporting every return code - without involving a DAW.
#
# The instantiation is the same call `asiolist.cpp:227` makes:
#   CoCreateInstance(clsid, 0, CLSCTX_INPROC_SERVER, riid = the same clsid, &p)
# and the calls that follow are the ones a host makes in order, at the vtable
# slots `common/iasiodrv.h` fixes (IUnknown = 0..2):
#
#   init 3, getDriverName 4, getDriverVersion 5, start 7, stop 8, getChannels 9,
#   getLatencies 10, getBufferSize 11, canSampleRate 12, getSampleRate 13,
#   setSampleRate 14, getClockSources 15, getChannelInfo 18, createBuffers 19,
#   disposeBuffers 20, Release 2
#
# A crash here is contained in this PowerShell process, so a mis-registered or
# half-implemented driver shows up as a named failing call instead of a crashed
# DAW. This is the instrument to run after every rebuild. Studio One records a
# failed device in AudioEngine.settings under `failedDevices`; this probe says
# *which call* returned the error behind that.
#
#   pwsh -File scripts/probe-asio-com.ps1
#   pwsh -File scripts/probe-asio-com.ps1 -Clsid '{...}' -BufferSize 512 -Channels 32
#   pwsh -File scripts/probe-asio-com.ps1 -DllPath dist\viola-asio-windows-x86_64\viola_asio.dll -ToneHz 1000 -Seconds 3
#
# Does not need elevation: reading the COM registration is enough.
[CmdletBinding()]
param(
    [string]$Clsid = '{6C1E7D94-3A52-4B8F-9E27-5D0B4C8A1F63}',
    # Load this DLL directly instead of going through the COM registration, so a
    # fresh build can be exercised before it is installed (installing needs an
    # elevated shell, and finding out whether a build is broken should not).
    [string]$DllPath,
    [int]$BufferSize = 512,
    # 16 in + 16 out, the bed the contract advertises.
    [int]$Channels = 32,
    # Play the DAW as well as probe it: write a distinct sine, base*(channel+1)
    # Hz, into every driver output while the stream runs. 0 keeps the probe the
    # read-only handshake the M5.2 verification uses.
    [int]$ToneHz = 0,
    # How long the driver is left to call back once it has been started.
    [int]$Seconds = 2
)

$ErrorActionPreference = 'Stop'

# Every vtable call happens inside C#. PowerShell can fetch a delegate but its
# binding of `.Invoke` on one is not dependable, so the probe keeps a thin
# static wrapper per slot instead.
$source = @'
using System;
using System.Runtime.InteropServices;

public static class AsioComProbe {
    [DllImport("ole32.dll", ExactSpelling = true, PreserveSig = true)]
    public static extern int CoCreateInstance(ref Guid rclsid, IntPtr pUnkOuter,
        uint dwClsContext, ref Guid riid, out IntPtr ppv);

    // ---- IASIO slots (IUnknown occupies 0..2) -------------------------------
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  InitD(IntPtr self, IntPtr sysHandle);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate void GetDriverNameD(IntPtr self, IntPtr name);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetDriverVersionD(IntPtr self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  StartD(IntPtr self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  StopD(IntPtr self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetChannelsD(IntPtr self, out int ins, out int outs);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetLatenciesD(IntPtr self, out int inLat, out int outLat);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetBufferSizeD(IntPtr self, out int min, out int max, out int pref, out int gran);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  CanSampleRateD(IntPtr self, double rate);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetSampleRateD(IntPtr self, out double rate);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  SetSampleRateD(IntPtr self, double rate);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetClockSourcesD(IntPtr self, IntPtr clocks, ref int numSources);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  GetChannelInfoD(IntPtr self, IntPtr info);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  CreateBuffersD(IntPtr self, IntPtr infos, int numChannels, int bufferSize, IntPtr callbacks);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  DisposeBuffersD(IntPtr self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate uint ReleaseD(IntPtr self);

    // ---- the four callbacks the driver calls back into ----------------------
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate void BufferSwitchD(int doubleBufferIndex, int directProcess);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate void SampleRateDidChangeD(double rate);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int  AsioMessageD(int selector, int value, IntPtr message, IntPtr opt);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate IntPtr BufferSwitchTimeInfoD(IntPtr parameters, int doubleBufferIndex, int directProcess);

    public static int SwitchCount;
    public static int TimeInfoCount;

    // Static fields, so the GC cannot collect them while the driver holds their
    // function pointers - a collected delegate is a crash inside the DAW.
    static readonly BufferSwitchD OnBufferSwitch = (index, direct) =>
        System.Threading.Interlocked.Increment(ref SwitchCount);
    static readonly SampleRateDidChangeD OnSampleRateDidChange = rate => { };
    static readonly AsioMessageD OnAsioMessage = (selector, value, message, opt) => 1;
    static readonly BufferSwitchTimeInfoD OnBufferSwitchTimeInfo = (parameters, index, direct) => {
        System.Threading.Interlocked.Increment(ref TimeInfoCount);
        // Play the DAW: the driver reads this half back as soon as we return.
        EmitTone(index);
        return IntPtr.Zero;
    };

    static IntPtr Slot(IntPtr p, int index) {
        IntPtr vtable = Marshal.ReadIntPtr(p);
        return Marshal.ReadIntPtr(vtable, index * IntPtr.Size);
    }
    static T Fn<T>(IntPtr p, int index) {
        return Marshal.GetDelegateForFunctionPointer<T>(Slot(p, index));
    }

    public static int  CallInit(IntPtr p, IntPtr sysHandle)                       { var d = Fn<InitD>(p, 3); return d(p, sysHandle); }
    public static void CallGetDriverName(IntPtr p, IntPtr buf)                    { var d = Fn<GetDriverNameD>(p, 4); d(p, buf); }
    public static int  CallGetDriverVersion(IntPtr p)                             { var d = Fn<GetDriverVersionD>(p, 5); return d(p); }
    public static int  CallStart(IntPtr p)                                        { var d = Fn<StartD>(p, 7); return d(p); }
    public static int  CallStop(IntPtr p)                                         { var d = Fn<StopD>(p, 8); return d(p); }
    public static int  CallGetChannels(IntPtr p, out int ins, out int outs)       { var d = Fn<GetChannelsD>(p, 9); return d(p, out ins, out outs); }
    public static int  CallGetLatencies(IntPtr p, out int a, out int b)           { var d = Fn<GetLatenciesD>(p, 10); return d(p, out a, out b); }
    public static int  CallGetBufferSize(IntPtr p, out int min, out int max, out int pref, out int gran) { var d = Fn<GetBufferSizeD>(p, 11); return d(p, out min, out max, out pref, out gran); }
    public static int  CallCanSampleRate(IntPtr p, double rate)                   { var d = Fn<CanSampleRateD>(p, 12); return d(p, rate); }
    public static int  CallGetSampleRate(IntPtr p, out double rate)               { var d = Fn<GetSampleRateD>(p, 13); return d(p, out rate); }
    public static int  CallSetSampleRate(IntPtr p, double rate)                   { var d = Fn<SetSampleRateD>(p, 14); return d(p, rate); }
    public static int  CallGetClockSources(IntPtr p, IntPtr clocks, ref int n)    { var d = Fn<GetClockSourcesD>(p, 15); return d(p, clocks, ref n); }
    public static int  CallGetChannelInfo(IntPtr p, IntPtr info)                  { var d = Fn<GetChannelInfoD>(p, 18); return d(p, info); }
    public static int  CallCreateBuffers(IntPtr p, IntPtr infos, int n, int size, IntPtr cb) { var d = Fn<CreateBuffersD>(p, 19); return d(p, infos, n, size, cb); }
    public static int  CallDisposeBuffers(IntPtr p)                               { var d = Fn<DisposeBuffersD>(p, 20); return d(p); }
    public static uint CallRelease(IntPtr p)                                      { var d = Fn<ReleaseD>(p, 2); return d(p); }

    // ---- the DAW side: writing tone into the driver's output buffers ---------
    // createBuffers overwrites the buffers[] we zeroed with the driver's own
    // memory, so the pointers have to be read back from the very block we hand
    // in. Each 24-byte ASIOBufferInfo holds two of them: buffers[0] at +8 and
    // buffers[1] at +16 (common/asio.h). A host fills the half the driver names
    // as the doubleBufferIndex.
    public static bool ToneEnabled;
    public static int  ToneFrames;
    public static int  OutputCount;
    // The probe sets the sample rate to 48000 unconditionally, and the contract
    // fixes 48000 as well.
    public const double ToneRate = 48000.0;

    static IntPtr    toneInfos;
    static float[][] toneBlocks;
    static double[]  tonePhases;
    static double[]  toneSteps;

    /// Preallocates everything the callback needs, because the callback itself
    /// must not allocate: a GC pause inside a DAW's audio thread is a dropout.
    public static void EnableTone(IntPtr infos, int channels, int bufferSize, int baseHz) {
        toneInfos   = infos;
        ToneFrames  = bufferSize;
        OutputCount = channels / 2;
        toneBlocks  = new float[OutputCount][];
        tonePhases  = new double[OutputCount];
        toneSteps   = new double[OutputCount];
        for (int o = 0; o < OutputCount; o++) {
            toneBlocks[o] = new float[bufferSize];
            tonePhases[o] = 0.0;
            // Channel N gets base*(N+1) Hz, so the channels stay distinguishable
            // even though analyze_raw_f32.py only reports per-channel levels.
            toneSteps[o] = 2.0 * Math.PI * (baseHz * (o + 1)) / ToneRate;
        }
        ToneEnabled = true;
    }

    /// Fills and copies one block of every output channel. `index` is the half
    /// the driver is about to have read back, so that is the half to write.
    public static void EmitTone(int index) {
        if (!ToneEnabled) return;
        int half = index & 1;
        for (int o = 0; o < OutputCount; o++) {
            float[] block = toneBlocks[o];
            double phase = tonePhases[o];
            double step  = toneSteps[o];
            for (int f = 0; f < ToneFrames; f++) {
                block[f] = (float)(0.25 * Math.Sin(phase));
                phase += step;
            }
            tonePhases[o] = phase % (2.0 * Math.PI);
            // Entry 2o+1 is the o-th output; the builder interleaves input, output.
            IntPtr dst = Marshal.ReadIntPtr(toneInfos, (2 * o + 1) * 24 + 8 + half * 8);
            if (dst != IntPtr.Zero) Marshal.Copy(block, 0, dst, ToneFrames);
        }
    }

    // ASIOBufferInfo: 24 bytes under #pragma pack(4) - long isInput, long channelNum, void* buffers[2].
    public static IntPtr BuildBufferInfos(int channels) {
        IntPtr block = Marshal.AllocHGlobal(channels * 24);
        for (int i = 0; i < channels; i++) {
            int offset = i * 24;
            Marshal.WriteInt32(block, offset + 0, (i % 2 == 0) ? 1 : 0);   // inputs first, then outputs
            Marshal.WriteInt32(block, offset + 4, i / 2);                  // channel number
            Marshal.WriteIntPtr(block, offset + 8, IntPtr.Zero);           // buffers[0] - the driver fills these in
            Marshal.WriteIntPtr(block, offset + 16, IntPtr.Zero);          // buffers[1]
        }
        return block;
    }

    // ASIOCallbacks: 32 bytes under pack(4) - four pointers at 0/8/16/24.
    public static IntPtr BuildCallbacks() {
        IntPtr block = Marshal.AllocHGlobal(32);
        Marshal.WriteIntPtr(block, 0,  Marshal.GetFunctionPointerForDelegate(OnBufferSwitch));
        Marshal.WriteIntPtr(block, 8,  Marshal.GetFunctionPointerForDelegate(OnSampleRateDidChange));
        Marshal.WriteIntPtr(block, 16, Marshal.GetFunctionPointerForDelegate(OnAsioMessage));
        Marshal.WriteIntPtr(block, 24, Marshal.GetFunctionPointerForDelegate(OnBufferSwitchTimeInfo));
        return block;
    }

    // ASIOChannelInfo: 52 bytes under pack(4).
    public static IntPtr BuildChannelInfo(int channel, int isInput) {
        IntPtr block = Marshal.AllocHGlobal(52);
        for (int i = 0; i < 52; i++) Marshal.WriteByte(block, i, 0);
        Marshal.WriteInt32(block, 0, channel);
        Marshal.WriteInt32(block, 4, isInput);
        return block;
    }

    // ASIOClockSource: 48 bytes under pack(4) - long index, long associatedChannel,
    // long associatedGroup, ASIOBool isCurrentSource, char name[32].
    public static IntPtr BuildClockSources(int count) {
        IntPtr block = Marshal.AllocHGlobal(count * 48);
        for (int i = 0; i < count * 48; i++) Marshal.WriteByte(block, i, 0);
        return block;
    }

    public static int ChannelInfoSampleType(IntPtr info) { return Marshal.ReadInt32(info, 16); }
    public static string ChannelInfoName(IntPtr info)    { return Marshal.PtrToStringAnsi(IntPtr.Add(info, 20)); }
    // ---- direct loading, for testing a build before it is installed ---------
    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr LoadLibraryW(string path);
    [DllImport("kernel32", SetLastError = true, CharSet = CharSet.Ansi)]
    static extern IntPtr GetProcAddress(IntPtr module, string name);

    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    delegate int DllGetClassObjectD(IntPtr rclsid, IntPtr riid, out IntPtr ppv);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)]
    delegate int CreateInstanceD(IntPtr self, IntPtr outer, IntPtr riid, out IntPtr ppv);

    /// Does by hand what CLSCTX_INPROC_SERVER does through the registry:
    /// LoadLibrary, DllGetClassObject, then the factory's CreateInstance with the
    /// CLSID handed in as the interface IID - exactly what asiolist.cpp does.
    public static IntPtr InstantiateFromDll(string dllPath, string clsid, ref string detail) {
        IntPtr module = LoadLibraryW(dllPath);
        if (module == IntPtr.Zero) {
            detail = "LoadLibrary failed, win32 error " + Marshal.GetLastWin32Error();
            return IntPtr.Zero;
        }
        IntPtr proc = GetProcAddress(module, "DllGetClassObject");
        if (proc == IntPtr.Zero) {
            detail = "the DLL does not export DllGetClassObject";
            return IntPtr.Zero;
        }

        Guid clsidGuid = new Guid(clsid);
        Guid iidClassFactory = new Guid("00000001-0000-0000-C000-000000000046");
        IntPtr clsidPtr = Marshal.AllocHGlobal(16);
        IntPtr iidPtr = Marshal.AllocHGlobal(16);
        Marshal.StructureToPtr(clsidGuid, clsidPtr, false);
        Marshal.StructureToPtr(iidClassFactory, iidPtr, false);

        IntPtr factory;
        var getClassObject = Marshal.GetDelegateForFunctionPointer<DllGetClassObjectD>(proc);
        int hr = getClassObject(clsidPtr, iidPtr, out factory);
        if (hr != 0 || factory == IntPtr.Zero) {
            detail = string.Format("DllGetClassObject returned 0x{0:X8}", hr);
            return IntPtr.Zero;
        }

        IntPtr instance;
        var createInstance = Marshal.GetDelegateForFunctionPointer<CreateInstanceD>(Slot(factory, 3));
        hr = createInstance(factory, IntPtr.Zero, clsidPtr, out instance);
        if (hr != 0 || instance == IntPtr.Zero) {
            detail = string.Format("IClassFactory::CreateInstance returned 0x{0:X8}", hr);
            return IntPtr.Zero;
        }
        detail = string.Format("DllGetClassObject ok, CreateInstance ok (0x{0:X8})", hr);
        return instance;
    }

    public static string AsioError(int code) {
        if (code == 0) return "ASE_OK";
        if (code == unchecked((int)0x3f4847a0)) return "ASE_SUCCESS";
        switch (code) {
            case -1000: return "ASE_NotPresent";
            case -999:  return "ASE_HWMalfunction";
            case -998:  return "ASE_InvalidParameter";
            case -997:  return "ASE_InvalidMode";
            case -996:  return "ASE_SPNotAdvancing";
            case -995:  return "ASE_NoClock";
            case -994:  return "ASE_NoMemory";
            default:    return "unknown";
        }
    }
}
'@
Add-Type -TypeDefinition $source -Language CSharp

function Format-Rc([int]$code) { "{0} ({1})" -f $code, [AsioComProbe]::AsioError($code) }
function Write-Line([string]$label, [string]$value) { Write-Host ("{0,-17}: {1}" -f $label, $value) }

$clsidGuid = [Guid]$Clsid
$unknown   = [IntPtr]::Zero
$clsidGuid = [Guid]$Clsid
$unknown   = [IntPtr]::Zero
if ($DllPath) {
    $detail = ''
    $unknown = [AsioComProbe]::InstantiateFromDll($DllPath, $Clsid, [ref]$detail)
    Write-Line 'CLSID' $Clsid
    Write-Line 'LoadLibrary' $(if ($unknown -eq [IntPtr]::Zero) { "FAILED - $detail" } else { "direct load - $detail" })
    if ($unknown -eq [IntPtr]::Zero) { exit 1 }
} else {
    $hr = [AsioComProbe]::CoCreateInstance([ref]$clsidGuid, [IntPtr]::Zero, 1, [ref]$clsidGuid, [ref]$unknown)
    Write-Line 'CLSID' $Clsid
    Write-Line 'CoCreateInstance' ("0x{0:X8} {1}" -f $hr, $(if ($hr -eq 0) { '(S_OK)' } else { '(FAILED)' }))
}
if ($unknown -eq [IntPtr]::Zero) {
    Write-Host ''
    Write-Host 'The driver could not be instantiated. Check, in this order:'
    Write-Host '  * HKLM\SOFTWARE\ASIO\<name> -> CLSID'
    Write-Host '  * HKLM\SOFTWARE\Classes\CLSID\{...}\InprocServer32 -> (default) = the DLL path'
    Write-Host '  * scripts/register-asio.ps1 with no arguments lists what is missing.'
    exit 1
}

# 1) init - a host treats ASIOFalse here as a hard stop.
$initResult = [AsioComProbe]::CallInit($unknown, [IntPtr]::Zero)
Write-Line 'init' ("{0} {1}" -f $initResult, $(if ($initResult -eq 1) { '(ASIOTrue)' } else { '(ASIOFalse - a host stops here)' }))

# 2) identity
$nameBuf = [Runtime.InteropServices.Marshal]::AllocHGlobal(32)
[AsioComProbe]::CallGetDriverName($unknown, $nameBuf)
$driverName = [Runtime.InteropServices.Marshal]::PtrToStringAnsi($nameBuf)
[Runtime.InteropServices.Marshal]::FreeHGlobal($nameBuf)
Write-Line 'getDriverName' ("""{0}""  version {1}" -f $driverName, [AsioComProbe]::CallGetDriverVersion($unknown))

# 3) channels
$ins = 0; $outs = 0
$rcChannels = [AsioComProbe]::CallGetChannels($unknown, [ref]$ins, [ref]$outs)
Write-Line 'getChannels' ("rc={0} in={1} out={2}" -f (Format-Rc $rcChannels), $ins, $outs)

# 4) buffer window, latencies, clock sources
$min = 0; $max = 0; $preferred = 0; $granularity = 0
$rcBuf = [AsioComProbe]::CallGetBufferSize($unknown, [ref]$min, [ref]$max, [ref]$preferred, [ref]$granularity)
Write-Line 'getBufferSize' ("rc={0} min={1} max={2} preferred={3} granularity={4}" -f (Format-Rc $rcBuf), $min, $max, $preferred, $granularity)

$inLat = 0; $outLat = 0
$rcLat = [AsioComProbe]::CallGetLatencies($unknown, [ref]$inLat, [ref]$outLat)
Write-Line 'getLatencies' ("rc={0} in={1} out={2}" -f (Format-Rc $rcLat), $inLat, $outLat)

$clockCount = 8
$clockBuf = [AsioComProbe]::BuildClockSources($clockCount)
$rcClock = [AsioComProbe]::CallGetClockSources($unknown, $clockBuf, [ref]$clockCount)
Write-Line 'getClockSources' ("rc={0} count={1}" -f (Format-Rc $rcClock), $clockCount)
[Runtime.InteropServices.Marshal]::FreeHGlobal($clockBuf)

# 5) sample rate
Write-Line 'canSampleRate' (Format-Rc ([AsioComProbe]::CallCanSampleRate($unknown, 48000.0)))
Write-Line 'setSampleRate' (Format-Rc ([AsioComProbe]::CallSetSampleRate($unknown, 48000.0)))
$rateValue = 0.0
$rcRate = [AsioComProbe]::CallGetSampleRate($unknown, [ref]$rateValue)
Write-Line 'getSampleRate' ("rc={0} value={1}" -f (Format-Rc $rcRate), $rateValue)

# 6) per-channel description - a host asks once per activated channel.
foreach ($probe in @(@(0, 1, 'input 0'), @(0, 0, 'output 0'))) {
    $infoBuf = [AsioComProbe]::BuildChannelInfo($probe[0], $probe[1])
    $rc = [AsioComProbe]::CallGetChannelInfo($unknown, $infoBuf)
    Write-Line 'getChannelInfo' ("{0,-8} rc={1} sampleType={2} name=""{3}""" -f $probe[2], (Format-Rc $rc), [AsioComProbe]::ChannelInfoSampleType($infoBuf), [AsioComProbe]::ChannelInfoName($infoBuf))
    [Runtime.InteropServices.Marshal]::FreeHGlobal($infoBuf)
}

# 7) the buffers themselves.
$infos = [AsioComProbe]::BuildBufferInfos($Channels)
$callbacks = [AsioComProbe]::BuildCallbacks()
$rcCreate = [AsioComProbe]::CallCreateBuffers($unknown, $infos, $Channels, $BufferSize, $callbacks)
Write-Line 'createBuffers' ("rc={0}  ({1} channels, {2} frames)" -f (Format-Rc $rcCreate), $Channels, $BufferSize)

if ($rcCreate -eq 0) {
    if ($ToneHz -gt 0) {
        # The driver filled buffers[0]/buffers[1] in during createBuffers, so the
        # pointers the callbacks write through are read back from that same block.
        [AsioComProbe]::EnableTone($infos, $Channels, $BufferSize, $ToneHz)
        Write-Line 'tone' ("{0} Hz base on {1} output channels, {2} Hz" -f $ToneHz, [AsioComProbe]::OutputCount, [AsioComProbe]::ToneRate)
    }
    # Only meaningful once the driver actually has buffers to switch.
    Write-Line 'start' (Format-Rc ([AsioComProbe]::CallStart($unknown)))
    Start-Sleep -Seconds $Seconds
    # asio.h offers two ways to tell the host, and the driver prefers the
    # time-info form when the host provided it, so both counters are reported:
    # a bare "bufferSwitch: 0" would read like silence when the cadence is fine.
    Write-Line 'bufferSwitch' ("called {0} times in {1} s (bufferSwitchTimeInfo: {2})" -f [AsioComProbe]::SwitchCount, $Seconds, [AsioComProbe]::TimeInfoCount)
    Write-Line 'stop' (Format-Rc ([AsioComProbe]::CallStop($unknown)))
    Write-Line 'disposeBuffers' (Format-Rc ([AsioComProbe]::CallDisposeBuffers($unknown)))
} else {
    Write-Line 'start/dispose' 'skipped, createBuffers did not succeed'
}

Write-Line 'Release' ("refcount {0}" -f [AsioComProbe]::CallRelease($unknown))
