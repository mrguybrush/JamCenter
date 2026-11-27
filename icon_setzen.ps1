<#
    Setzt app/jamcenter.ico als Programmsymbol in eine fertig gebaute exe (Ressourcen RT_ICON + RT_GROUP_ICON, ID 1).
    Nötig, weil die GNU-Toolchain keinen Ressourcen-Compiler (rc/windres) mitbringt. Die übrigen Ressourcen (Manifest) bleiben.
    Aufruf:  powershell -NoProfile -File icon_setzen.ps1 [-Exe target/release/jamcenter.exe]
#>
param([string]$Exe = "$PSScriptRoot\target\release\jamcenter.exe", [string]$Ico = "$PSScriptRoot\app\jamcenter.ico")
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
public static class Symbol {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern IntPtr BeginUpdateResource(string datei, bool loeschen);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool UpdateResource(IntPtr h, IntPtr typ, IntPtr name, ushort sprache, byte[] daten, uint groesse);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool EndUpdateResource(IntPtr h, bool verwerfen);

    public static int Setzen(string exe, string ico) {
        byte[] d = File.ReadAllBytes(ico);
        int anzahl = BitConverter.ToUInt16(d, 4);
        IntPtr h = BeginUpdateResource(exe, false);
        if (h == IntPtr.Zero) throw new Exception("BeginUpdateResource: " + Marshal.GetLastWin32Error());
        var gruppe = new MemoryStream();
        var w = new BinaryWriter(gruppe);
        w.Write((ushort)0); w.Write((ushort)1); w.Write((ushort)anzahl);
        for (int i = 0; i < anzahl; i++) {
            int e = 6 + i * 16;
            uint groesse = BitConverter.ToUInt32(d, e + 8);
            uint start = BitConverter.ToUInt32(d, e + 12);
            byte[] bild = new byte[groesse];
            Array.Copy(d, start, bild, 0, groesse);
            // RT_ICON = 3, Sprache neutral
            if (!UpdateResource(h, (IntPtr)3, (IntPtr)(i + 1), 0, bild, groesse)) throw new Exception("RT_ICON: " + Marshal.GetLastWin32Error());
            w.Write(d, e, 12);            // Breite, Höhe, Farben, reserviert, Ebenen, Bits, Größe
            w.Write((ushort)(i + 1));     // ID statt Dateiversatz
        }
        byte[] g = gruppe.ToArray();
        // RT_GROUP_ICON = 14, ID 1 (LoadIcon(hinst, MAKEINTRESOURCE(1)))
        if (!UpdateResource(h, (IntPtr)14, (IntPtr)1, 0, g, (uint)g.Length)) throw new Exception("RT_GROUP_ICON: " + Marshal.GetLastWin32Error());
        if (!EndUpdateResource(h, false)) throw new Exception("EndUpdateResource: " + Marshal.GetLastWin32Error());
        return anzahl;
    }
}
'@
$n = [Symbol]::Setzen((Resolve-Path $Exe).Path, (Resolve-Path $Ico).Path)
Write-Host "Symbol gesetzt ($n Groessen): $Exe"
