# Raw Input mouse probe for a Windows host (GPL-3.0, part of Pulsar).
# Self-test (no args): injects 5 relative SendInput moves and logs what Raw Input reports.
# -Listen <ms>: only logs every WM_INPUT mouse packet (device, usFlags, deltas, cursor) for
#   that long, without injecting; run it while reproducing a mouse bug in a game.
# -Listen -1: SetCursorPos/recenter loop (what cursor-recentering games do).
# Run inside the target session, e.g. as a scheduled task with /it /ru <user>.
param([string]$Out = "C:\Users\Public\rawtest.txt", [int]$Listen = 0)
$ErrorActionPreference = 'Stop'
$src = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;
using System.Windows.Forms;
public class RawTest : NativeWindow {
  [StructLayout(LayoutKind.Sequential)] struct RID { public ushort page, usage; public uint flags; public IntPtr hwnd; }
  [StructLayout(LayoutKind.Sequential)] struct MI { public int dx, dy; public uint data, flags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public MI mi; }
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int x, y; }
  [DllImport("user32.dll")] static extern bool RegisterRawInputDevices(RID[] d, uint n, uint sz);
  [DllImport("user32.dll")] static extern uint GetRawInputData(IntPtr h, uint cmd, byte[] buf, ref uint sz, uint hsz);
  [DllImport("user32.dll")] static extern uint GetRawInputDeviceInfoW(IntPtr h, uint cmd, char[] buf, ref uint sz);
  [DllImport("user32.dll")] static extern uint SendInput(uint n, INPUT[] i, int sz);
  [DllImport("user32.dll")] static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int i);
  public List<string> Log = new List<string>();
  Dictionary<IntPtr,string> names = new Dictionary<IntPtr,string>();
  public RawTest() {
    CreateHandle(new CreateParams());
    var r = new RID[] { new RID { page = 1, usage = 2, flags = 0x100, hwnd = Handle } };
    if (!RegisterRawInputDevices(r, 1, (uint)Marshal.SizeOf(typeof(RID)))) Log.Add("register failed " + Marshal.GetLastWin32Error());
  }
  string Name(IntPtr h) {
    if (h == IntPtr.Zero) return "(null hDevice = injected)";
    string n; if (names.TryGetValue(h, out n)) return n;
    uint sz = 512; var b = new char[512];
    GetRawInputDeviceInfoW(h, 0x20000007, b, ref sz);
    n = new string(b).TrimEnd('\0'); names[h] = n; return n;
  }
  protected override void WndProc(ref Message m) {
    if (m.Msg == 0xFF) {
      uint sz = 0; uint hs = (uint)(IntPtr.Size == 8 ? 24 : 16);
      GetRawInputData(m.LParam, 0x10000003, null, ref sz, hs);
      var b = new byte[sz];
      GetRawInputData(m.LParam, 0x10000003, b, ref sz, hs);
      IntPtr dev = IntPtr.Size == 8 ? (IntPtr)BitConverter.ToInt64(b, 8) : (IntPtr)BitConverter.ToInt32(b, 8);
      int o = (int)hs;
      ushort fl = BitConverter.ToUInt16(b, o);
      ushort bf = BitConverter.ToUInt16(b, o + 4);
      int x = BitConverter.ToInt32(b, o + 12), y = BitConverter.ToInt32(b, o + 16);
      uint extra = BitConverter.ToUInt32(b, o + 20);
      POINT cp; GetCursorPos(out cp); Log.Add(String.Format("{0:HH:mm:ss.fff} usFlags=0x{1:X2} btn=0x{2:X4} lLastX={3} lLastY={4} extra=0x{5:X} cursor={7},{8} dev={6}", DateTime.Now, fl, bf, x, y, extra, Name(dev), cp.x, cp.y));
    }
    base.WndProc(ref m);
  }
  public static void Rel(int dx, int dy) {
    var i = new INPUT[] { new INPUT { type = 0, mi = new MI { dx = dx, dy = dy, flags = 1 } } };
    uint ok = SendInput(1, i, Marshal.SizeOf(typeof(INPUT))); if (ok != 1) throw new Exception("SendInput failed " + Marshal.GetLastWin32Error());
  }
  public static string Run(int listenMs) {
    var t = new RawTest();
    POINT p0; GetCursorPos(out p0);
    t.Log.Add(String.Format("SM_REMOTESESSION={0} cursor={1},{2} ptrsize={3} inputsz={4}", GetSystemMetrics(0x1000), p0.x, p0.y, IntPtr.Size, Marshal.SizeOf(typeof(INPUT))));
    var timer = new System.Windows.Forms.Timer(); int step = 0;
    timer.Interval = 30;
    timer.Tick += (s, e) => {
      step++;
      if (listenMs < 0) {
        POINT q;
        if (step == 2) { SetCursorPos(960, 540); GetCursorPos(out q); t.Log.Add("-- SetCursorPos(960,540) -> " + q.x + "," + q.y); }
        if (step >= 4 && step < 12) {
          Rel(5, 0); GetCursorPos(out q); t.Log.Add("-- Rel(5,0) -> cursor " + q.x + "," + q.y);
          SetCursorPos(960, 540); GetCursorPos(out q); t.Log.Add("-- recenter -> cursor " + q.x + "," + q.y);
        }
        if (step == 14) SetCursorPos(p0.x, p0.y);
        if (step == 20) Application.ExitThread();
      } else if (listenMs == 0) {
        if (step >= 5 && step < 10) { t.Log.Add("-- SendInput MOVE +7,+3"); Rel(7, 3); }
        if (step >= 10 && step < 15) { t.Log.Add("-- SendInput MOVE -7,-3"); Rel(-7, -3); }
        if (step == 16) SetCursorPos(p0.x, p0.y);
        if (step == 25) Application.ExitThread();
      } else if (step * 30 > listenMs) Application.ExitThread();
    };
    timer.Start();
    Application.Run();
    POINT p1; GetCursorPos(out p1);
    t.Log.Add(String.Format("end cursor={0},{1}", p1.x, p1.y));
    return String.Join("\r\n", t.Log.ToArray());
  }
}
'@
try {
  Add-Type -TypeDefinition $src -ReferencedAssemblies System.Windows.Forms
  $r = [RawTest]::Run($Listen)
  $hdr = "user=$env:USERNAME session=" + [System.Diagnostics.Process]::GetCurrentProcess().SessionId + " SESSIONNAME=$env:SESSIONNAME"
  ($hdr + "`r`n" + $r) | Out-File -Encoding utf8 $Out
} catch { $_ | Out-String | Out-File -Encoding utf8 $Out }
