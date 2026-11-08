using System;
using System.IO;
using Microsoft.Win32;

namespace NiwCam.Services;

// Where the app keeps its files, and the one-time move from the names used before 0.1.14, when the
// app was called "Cam Studio" (folder %LOCALAPPDATA%\CamStudio, settings HKCU\Software\CamStudio,
// autostart value "CamStudio"). Init() runs first thing in Main, before the engine or any window.
//
// The pairing keys of every phone live in this folder: a move that goes wrong must never leave an
// empty new folder next to the old one (a new client id and no keys: every phone asks for its 6-digit
// code again). So the folder is moved whole, and when that fails the old one keeps being used.
public static class AppData
{
    public const string FolderName = "NiwCam";
    private const string OldFolderName = "CamStudio";
    private const string OldSettingsKey = @"Software\CamStudio";
    private const string NewSettingsKey = @"Software\NiwCam";
    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";

    private static readonly Lazy<string> _dir = new(Resolve);

    /// The data folder in use (exists once this is read).
    public static string Dir => _dir.Value;

    public static string File(string name) => Path.Combine(Dir, name);

    public static void Init()
    {
        _ = Dir;
        Try(MigrateSettings);
        Try(MigrateAutostart);
    }

    private static string Resolve()
    {
        string local = Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData);
        string fresh = Path.Combine(local, FolderName);
        string old = Path.Combine(local, OldFolderName);
        string result = fresh;
        try
        {
            if (Directory.Exists(old))
            {
                if (!Directory.Exists(fresh))
                {
                    Directory.Move(old, fresh);
                }
                else if (!HasFiles(fresh))
                {
                    // Only empty folders: something (a test tool) made it before the move.
                    Directory.Delete(fresh, recursive: true);
                    Directory.Move(old, fresh);
                }
                // Both hold files: the new folder wins, the old one is left alone.
            }
        }
        catch (Exception)
        {
            // Files in use, or no rights. Keep using the folder that holds the keys.
            if (Directory.Exists(old) && (!Directory.Exists(fresh) || !HasFiles(fresh))) result = old;
        }
        try
        {
            Directory.CreateDirectory(result);
        }
        catch (Exception)
        {
        }
        // The engine (C++) uses the same folder.
        Environment.SetEnvironmentVariable("CAMSTUDIO_DATA_DIR", result);
        return result;
    }

    private static bool HasFiles(string dir)
    {
        try
        {
            foreach (var _ in Directory.EnumerateFiles(dir, "*", SearchOption.AllDirectories)) return true;
        }
        catch (Exception)
        {
            return true; // cannot tell: treat it as holding something
        }
        return false;
    }

    // The old settings are copied, not moved: they stay for anyone who goes back to an older build.
    private static void MigrateSettings()
    {
        using var old = Registry.CurrentUser.OpenSubKey(OldSettingsKey);
        if (old is null) return;
        using (var fresh = Registry.CurrentUser.OpenSubKey(NewSettingsKey))
            if (fresh is not null) return;
        using var target = Registry.CurrentUser.CreateSubKey(NewSettingsKey);
        CopyKey(old, target);
    }

    private static void CopyKey(RegistryKey from, RegistryKey to)
    {
        foreach (string name in from.GetValueNames())
        {
            var kind = from.GetValueKind(name);
            object? value = from.GetValue(name, null, RegistryValueOptions.DoNotExpandEnvironmentNames);
            if (value is not null) to.SetValue(name, value, kind);
        }
        foreach (string sub in from.GetSubKeyNames())
        {
            using var s = from.OpenSubKey(sub);
            if (s is null) continue;
            using var t = to.CreateSubKey(sub);
            CopyKey(s, t);
        }
    }

    // The Run value pointed at CamStudio.exe, which an update removes: write ours (this exe) instead.
    private static void MigrateAutostart()
    {
        using var run = Registry.CurrentUser.CreateSubKey(RunKey);
        if (run.GetValue(OldFolderName) is not string old) return;
        if (run.GetValue(FolderName) is null)
        {
            string args = old.Contains("--tray", StringComparison.OrdinalIgnoreCase) ? " --tray" : "";
            run.SetValue(FolderName, $"\"{Environment.ProcessPath}\"{args}");
        }
        run.DeleteValue(OldFolderName, throwOnMissingValue: false);
    }

    private static void Try(Action action)
    {
        try
        {
            action();
        }
        catch (Exception)
        {
        }
    }
}
