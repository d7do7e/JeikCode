!macro JEIKCODE_KILL_INSTDIR_BINARIES
  ; Terminate only JeikCode binaries running from $INSTDIR, never unrelated workspaces or installer ancestors
  System::Call 'Kernel32::SetEnvironmentVariable(t "JEIKCODE_NSIS_INSTDIR", t "$INSTDIR")'
  nsExec::Exec 'powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$$root = $$env:JEIKCODE_NSIS_INSTDIR.TrimEnd([char]92) + [char]92; $$names = @(\"jeikcode-desktop.exe\",\"jeikcode.exe\",\"jeikcode-daemon.exe\",\"cli-bin.exe\"); $$procs = Get-CimInstance Win32_Process; $$byPid = @{}; foreach ($$p in $$procs) { $$byPid[[int]$$p.ProcessId] = $$p }; $$skip = @{}; $$cur = [int]$$PID; while ($$cur -gt 0 -and -not $$skip.ContainsKey($$cur)) { $$skip[$$cur] = $$true; if ($$byPid.ContainsKey($$cur)) { $$cur = [int]$$byPid[$$cur].ParentProcessId } else { break } }; foreach ($$p in $$procs) { if (-not $$skip.ContainsKey([int]$$p.ProcessId) -and $$p.Name -and ($$names -contains $$p.Name.ToLowerInvariant()) -and $$p.ExecutablePath -and $$p.ExecutablePath.StartsWith($$root, [System.StringComparison]::OrdinalIgnoreCase)) { Stop-Process -Id ([int]$$p.ProcessId) -Force -ErrorAction SilentlyContinue } }"'
  System::Call 'Kernel32::SetEnvironmentVariable(t "JEIKCODE_NSIS_INSTDIR", n)'
  Sleep 800
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro JEIKCODE_KILL_INSTDIR_BINARIES

  ; Clean up potentially locked old binaries in installation directory
  IfFileExists "$INSTDIR\resources\jeikcode.exe" 0 +2
    Delete "$INSTDIR\resources\jeikcode.exe"
  IfFileExists "$INSTDIR\resources\cli-bin" 0 +2
    Delete "$INSTDIR\resources\cli-bin"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Re-create desktop shortcut explicitly with executable icon index 0
  CreateShortCut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe" "" "$INSTDIR\${MAINBINARYNAME}.exe" 0
  ; Refresh shell icon cache so existing cached placeholders/icons are cleared
  System::Call 'shell32.dll::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'

  ; Register $INSTDIR and $INSTDIR\resources into user PATH via winreg (avoids NSIS 1024-char ReadRegStr truncation)
  nsExec::Exec '"$INSTDIR\${MAINBINARYNAME}.exe" --register-path'

  ; Clean up legacy ~/.local/bin files if left by older dropper versions
  IfFileExists "$PROFILE\.local\bin\jeikcode.exe" 0 +2
    Delete "$PROFILE\.local\bin\jeikcode.exe"
  IfFileExists "$PROFILE\.local\bin\jeikcode.bak.exe" 0 +2
    Delete "$PROFILE\.local\bin\jeikcode.bak.exe"
  IfFileExists "$PROFILE\.local\bin\atomcode.exe" 0 +2
    Delete "$PROFILE\.local\bin\atomcode.exe"
  IfFileExists "$PROFILE\.local\bin\atomcode.bak.exe" 0 +2
    Delete "$PROFILE\.local\bin\atomcode.bak.exe"
  RMDir "$PROFILE\.local\bin"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro JEIKCODE_KILL_INSTDIR_BINARIES
!macroend

!macro NSIS_HOOK_UNINSTALL
  ; Clean up desktop shortcut
  Delete "$DESKTOP\${PRODUCTNAME}.lnk"
  ; Refresh shell icon cache
  System::Call 'shell32.dll::SHChangeNotify(i 0x08000000, i 0, p 0, p 0)'
!macroend
