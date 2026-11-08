using System;
using System.Runtime.InteropServices;

namespace NiwCam.Services;

// The small red dot on the window's taskbar button while an app uses the camera
// (ITaskbarList3::SetOverlayIcon, the same badge chat apps use for a call). The interface is called
// through its vtable: it is three methods out of a long list, and an interop assembly is not worth it.
public sealed unsafe partial class TaskbarOverlay : IDisposable
{
    private const int HrInit = 3, SetOverlayIconSlot = 18; // vtable slots of ITaskbarList3
    private nint _list;
    private readonly nint _hwnd;
    private readonly nint _dot;
    private bool _on;

    public TaskbarOverlay(nint hwnd, string dotIconPath)
    {
        _hwnd = hwnd;
        int size = GetSystemMetrics(49 /* SM_CXSMICON */);
        fixed (char* p = dotIconPath) _dot = LoadImageW(0, p, 1 /* IMAGE_ICON */, size, size, 0x10 /* LR_LOADFROMFILE */);
        Guid clsid = new("56FDF344-FD6D-11d0-958A-006097C9A090"); // CLSID_TaskbarList
        Guid iid = new("EA1AFB91-9E28-4B86-90E9-9E9F8A5EEFAF");   // IID_ITaskbarList3
        if (CoCreateInstance(&clsid, 0, 1 /* CLSCTX_INPROC_SERVER */, &iid, out nint list) != 0 || list == 0) return;
        nint* vtbl = *(nint**)list;
        if (((delegate* unmanaged[Stdcall]<nint, int>)vtbl[HrInit])(list) >= 0) _list = list;
        else ((delegate* unmanaged[Stdcall]<nint, uint>)vtbl[2])(list);
    }

    /// Shows or clears the dot. False while the window has no taskbar button yet (hidden in the
    /// tray): ask again later. `force` puts it back after Windows made the button again.
    public bool Set(bool on, string description, bool force = false)
    {
        if (_list == 0 || _dot == 0) return false;
        if (on == _on && !force) return true;
        nint* vtbl = *(nint**)_list;
        fixed (char* d = description)
        {
            int hr = ((delegate* unmanaged[Stdcall]<nint, nint, nint, char*, int>)vtbl[SetOverlayIconSlot])(_list, _hwnd, on ? _dot : 0, d);
            if (hr < 0) return false;
        }
        _on = on;
        return true;
    }

    public void Dispose()
    {
        if (_list != 0)
        {
            nint* vtbl = *(nint**)_list;
            ((delegate* unmanaged[Stdcall]<nint, uint>)vtbl[2])(_list);
            _list = 0;
        }
        if (_dot != 0) DestroyIcon(_dot);
    }

    [LibraryImport("ole32.dll")] private static partial int CoCreateInstance(Guid* clsid, nint outer, uint context, Guid* iid, out nint result);
    [LibraryImport("user32.dll")] private static partial int GetSystemMetrics(int index);
    [LibraryImport("user32.dll")] private static partial nint LoadImageW(nint inst, char* name, uint type, int cx, int cy, uint flags);
    [LibraryImport("user32.dll")] private static partial int DestroyIcon(nint icon);
}
