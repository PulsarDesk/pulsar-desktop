# DirectInput8 mouse probe for a Windows host (GPL-3.0, part of Pulsar).
# Opens the system mouse (background, non-exclusive, relative data format), reports
# DIPROP_AXISMODE and GetDeviceState while it injects relative SendInput moves
# (-Moves 0: no injection, just poll). Run inside the target session.
param([string]$Out = "C:\Users\Public\ditest.txt", [int]$Moves = 1)
$src = @'
using System; using System.Runtime.InteropServices; using System.Text; using System.Threading; using System.Windows.Forms;
public class DiTest : NativeWindow {
  [DllImport("dinput8.dll")] static extern int DirectInput8Create(IntPtr hinst, uint ver, ref Guid iid, out IntPtr di, IntPtr outer);
  [DllImport("kernel32.dll")] static extern IntPtr GetModuleHandleW(IntPtr n);
  [StructLayout(LayoutKind.Sequential)] struct MI { public int dx, dy; public uint data, flags, time; public IntPtr extra; }
  [StructLayout(LayoutKind.Sequential)] struct INPUT { public uint type; public MI mi; }
  [DllImport("user32.dll")] static extern uint SendInput(uint n, INPUT[] i, int sz);
  [DllImport("user32.dll")] static extern bool GetCursorPos(out long p);
  [DllImport("user32.dll")] static extern bool SetCursorPos(int x, int y);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int CreateDev(IntPtr self, ref Guid g, out IntPtr dev, IntPtr outer);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int GetProp(IntPtr self, IntPtr guid, byte[] hdr);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int NoArg(IntPtr self);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int GetState(IntPtr self, int n, int[] st);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetFmt(IntPtr self, IntPtr fmt);
  [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int SetCoop(IntPtr self, IntPtr hwnd, int fl);
  static T V<T>(IntPtr obj, int i) { IntPtr vt = Marshal.ReadIntPtr(obj); return (T)(object)Marshal.GetDelegateForFunctionPointer(Marshal.ReadIntPtr(vt, i * IntPtr.Size), typeof(T)); }
  static IntPtr G(string s) { var p = Marshal.AllocHGlobal(16); Marshal.Copy(new Guid(s).ToByteArray(), 0, p, 16); return p; }
  static void Rel(int dx, int dy) { var i = new INPUT[] { new INPUT { type = 0, mi = new MI { dx = dx, dy = dy, flags = 1 } } }; SendInput(1, i, Marshal.SizeOf(typeof(INPUT))); }
  public static string Run(int moves) {
    var sb = new StringBuilder(); var w = new DiTest(); w.CreateHandle(new CreateParams());
    var iid = new Guid("BF798031-483A-4DA2-AA99-5D64ED369700"); IntPtr di;
    int hr = DirectInput8Create(GetModuleHandleW(IntPtr.Zero), 0x0800, ref iid, out di, IntPtr.Zero); sb.AppendLine("DirectInput8Create hr=" + hr.ToString("X"));
    var mouse = new Guid("6F1D2B60-D5A0-11CF-BFC7-444553540000"); IntPtr dev;
    hr = V<CreateDev>(di, 3)(di, ref mouse, out dev, IntPtr.Zero); sb.AppendLine("CreateDevice hr=" + hr.ToString("X"));
    // DIOBJECTDATAFORMAT x3 (X, Y, Z) + DIDATAFORMAT (relative axes, 16-byte DIMOUSESTATE)
    IntPtr objs = Marshal.AllocHGlobal(24 * 3); string[] ax = { "A36D02E0-C9F3-11CF-BFC7-444553540000", "A36D02E1-C9F3-11CF-BFC7-444553540000", "A36D02E2-C9F3-11CF-BFC7-444553540000" };
    for (int i = 0; i < 3; i++) { Marshal.WriteIntPtr(objs, i * 24, G(ax[i])); Marshal.WriteInt32(objs, i * 24 + 8, i * 4); Marshal.WriteInt32(objs, i * 24 + 12, unchecked((int)(0x00FFFF03u | (i == 2 ? 0x80000000u : 0)))); Marshal.WriteInt32(objs, i * 24 + 16, 0); }
    IntPtr fmt = Marshal.AllocHGlobal(32); Marshal.WriteInt32(fmt, 0, 32); Marshal.WriteInt32(fmt, 4, 24); Marshal.WriteInt32(fmt, 8, 2); Marshal.WriteInt32(fmt, 12, 16); Marshal.WriteInt32(fmt, 16, 3); Marshal.WriteIntPtr(fmt, 24, objs);
    hr = V<SetFmt>(dev, 11)(dev, fmt); sb.AppendLine("SetDataFormat(rel) hr=" + hr.ToString("X"));
    hr = V<SetCoop>(dev, 13)(dev, w.Handle, 0x8 | 0x2); sb.AppendLine("SetCooperativeLevel(background|nonexclusive) hr=" + hr.ToString("X"));
    var p = new byte[20]; BitConverter.GetBytes(20).CopyTo(p, 0); BitConverter.GetBytes(16).CopyTo(p, 4);
    hr = V<GetProp>(dev, 5)(dev, (IntPtr)2, p); sb.AppendLine("DIPROP_AXISMODE hr=" + hr.ToString("X") + " value=" + BitConverter.ToInt32(p, 16) + " (0=ABS 1=REL)");
    hr = V<NoArg>(dev, 7)(dev); sb.AppendLine("Acquire hr=" + hr.ToString("X"));
    var st = new int[4];
    int step = 0; var timer = new System.Windows.Forms.Timer(); timer.Interval = 40;
    timer.Tick += (s, e) => {
      step++;
      if (step >= 3 && step < 3 + 10 && moves != 0) { Rel(step < 8 ? 6 : -6, 0); }
      int h2 = V<GetState>(dev, 9)(dev, 16, st); long c; GetCursorPos(out c);
      sb.AppendLine(String.Format("tick {0} {1} GetDeviceState hr={2:X} lX={3} lY={4} lZ={5} cursor={6},{7}", step, (step >= 3 && step < 13 && moves != 0) ? (step < 8 ? "after +6" : "after -6") : "idle", h2, st[0], st[1], st[2], (int)c, (int)(c >> 32)));
      if (step == 18) Application.ExitThread();
    };
    timer.Start(); Application.Run();
    V<NoArg>(dev, 8)(dev);
    return sb.ToString();
  }
}
'@
try { Add-Type -TypeDefinition $src -ReferencedAssemblies System.Windows.Forms
  $hdr = "user=$env:USERNAME session=" + [System.Diagnostics.Process]::GetCurrentProcess().SessionId
  ($hdr + "`r`n" + [DiTest]::Run($Moves)) | Out-File -Encoding utf8 $Out } catch { $_ | Out-String | Out-File -Encoding utf8 $Out }
