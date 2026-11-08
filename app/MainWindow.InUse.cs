using System;
using System.Collections.Generic;
using System.IO;
using Microsoft.UI.Xaml;
using NiwCam.Native;
using NiwCam.Services;

namespace NiwCam;

// "In use" signs, like the red dot of a call app: while an app uses the virtual camera the tray icon
// carries a red dot and so does the taskbar button, and the tooltip names the app. Hidden in the
// tray, a quiet notification says so once when the use starts (Settings can switch it off).
public sealed partial class MainWindow
{
    private const int InUseHoldMs = 4000; // apps restart their stream to change mode: do not blink
    private bool _inUseLit;
    private DateTime _inUseSince, _inUseLastSeen;
    private bool _inUseBalloonShown;
    private List<string> _inUseApps = new();
    private TaskbarOverlay? _overlay;

    private void SetupInUseIndicator()
    {
        try
        {
            _overlay = new TaskbarOverlay(WinRT.Interop.WindowNative.GetWindowHandle(this),
                Path.Combine(AppContext.BaseDirectory, "Assets", "NiwCam-dot.ico"));
        }
        catch (Exception)
        {
            _overlay = null; // the tray icon still shows it
        }
        bool was = _loading;
        _loading = true;
        InUseNoticeToggle.IsOn = _settings.InUseNotice;
        _loading = was;
    }

    // Once a second, from UpdateStatus, before the tooltip is set.
    private void UpdateInUseIndicator(CsStatus s, EngineState state)
    {
        var now = DateTime.UtcNow;
        bool inUse = state == EngineState.Running && s.ConsumerActive != 0;
        if (inUse)
        {
            _inUseLastSeen = now;
            if (!_inUseLit)
            {
                _inUseLit = true;
                _inUseSince = now;
                _inUseBalloonShown = false;
            }
        }
        else if (_inUseLit && (now - _inUseLastSeen).TotalMilliseconds > InUseHoldMs)
        {
            _inUseLit = false;
            _inUseApps.Clear();
        }

        if (_inUseLit && _statusTicks % 3 == 0) _inUseApps = CameraUsage.RunningAppsInUse(); // the registry: not every tick

        _tray.SetLive(_inUseLit);
        _overlay?.Set(_inUseLit, "NiwCam đang được dùng", force: _inUseLit && _statusTicks % 3 == 0);

        // Hidden, with an app on the camera: one quiet notice, once the app's name is known (or after a few seconds).
        if (_inUseLit && !_inUseBalloonShown && _settings.InUseNotice && !IsOnScreen && !_recording)
        {
            double seconds = (now - _inUseSince).TotalSeconds;
            if (seconds >= 2 && (_inUseApps.Count > 0 || seconds >= 6))
            {
                _inUseBalloonShown = true;
                string who = _inUseApps.Count > 0 ? string.Join(", ", _inUseApps) : "Một ứng dụng";
                _tray.ShowBalloon("NiwCam đang được dùng", $"{who} đang dùng camera ảo ({s.Width} × {s.Height}).", silent: true);
            }
        }
    }

    // The tray tooltip says who, while the dot is lit.
    private string? InUseTooltip(CsStatus s)
    {
        if (!_inUseLit) return null;
        string who = _inUseApps.Count > 0 ? string.Join(", ", _inUseApps) : "Một ứng dụng";
        string tip = $"NiwCam · {who} đang dùng camera · {s.Width} × {s.Height}";
        return tip.Length > 120 ? tip[..119] + "…" : tip;
    }

    private void InUseNoticeToggle_Toggled(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        _settings.InUseNotice = InUseNoticeToggle.IsOn;
        _settings.Save();
    }
}
