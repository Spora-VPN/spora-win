; Spora NSIS Installer Script
; Expects defines passed from build script:
;   WINUI_EXE  - path to spora-winui.exe
;   WINCORE_EXE - path to spora-wincore.exe
;   WINTUN_DLL - path to wintun.dll
;   ICON_FILE   - path to spora.ico
;   OUTFILE_PATH - output installer exe path
;   VERSION     - release version (the workspace version, e.g. 0.2.1)
;   VERSION_NUMERIC - the same as four numeric fields (0.2.1.0) for
;                 the installer's own version resource

!include "MUI2.nsh"
!include "LogicLib.nsh"

; ── General ──────────────────────────────────────────────────────────

Name "Spora"
OutFile "${OUTFILE_PATH}"
InstallDir "$PROGRAMFILES64\Spora"
InstallDirRegKey HKLM "Software\Spora" "InstallDir"
RequestExecutionLevel admin

!ifndef VERSION
    !error "VERSION must be defined (build with build-winui.sh)"
!endif
!ifndef VERSION_NUMERIC
    !error "VERSION_NUMERIC must be defined (build with build-winui.sh)"
!endif

; The installer's own file properties (Explorer's Details tab). No
; LegalCopyright until the client has a licence of its own; silence the
; "without standard key" warning that leaves.
!pragma warning disable 9100
VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey "ProductName" "Spora"
VIAddVersionKey "FileDescription" "Spora Setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"

; ── Interface ────────────────────────────────────────────────────────

!define MUI_ICON "${ICON_FILE}"
!define MUI_UNICON "${ICON_FILE}"
!define MUI_ABORTWARNING

; ── Pages ────────────────────────────────────────────────────────────

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!define MUI_FINISHPAGE_RUN "$INSTDIR\spora-winui.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch Spora"
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ── Install Sections ─────────────────────────────────────────────────

Section "Spora (required)" SecMain
    SectionIn RO

    ; Kill any running UI so its exe can be replaced (all install types).
    nsExec::ExecToLog 'taskkill.exe /F /IM spora-winui.exe'
    Pop $0

    ; If a previous service-era install exists, this is an upgrade. Clear its
    ; restart-on-failure actions FIRST so the taskkill fallback below can't
    ; trip an SCM auto-restart mid-extraction, then stop it and wait for it to
    ; actually report STOPPED before we overwrite its files. (Old helper-era
    ; installs had no service; sc query returns 1060 and we skip this.)
    nsExec::ExecToLog 'sc.exe query Spora'
    Pop $0
    ${If} $0 == 0
        nsExec::ExecToLog 'sc.exe failure Spora reset= 0 actions= ""'
        Pop $0
        nsExec::ExecToLog 'sc.exe stop Spora'
        Pop $0
        StrCpy $1 0
        wincore_wait:
            nsExec::ExecToStack 'cmd /c sc query Spora | findstr STOPPED'
            Pop $0   ; findstr exit code (0 = matched STOPPED)
            Pop $2   ; captured output (discarded)
            ${If} $0 == 0
                Goto wincore_stopped
            ${EndIf}
            IntOp $1 $1 + 1
            ${If} $1 >= 20
                Goto wincore_stopped
            ${EndIf}
            Sleep 1000
            Goto wincore_wait
        wincore_stopped:
    ${EndIf}

    ; Belt-and-suspenders: kill any lingering wincore (a helper-era process,
    ; or a service that didn't stop within the wait) so its exe/dll aren't
    ; locked when we extract.
    nsExec::ExecToLog 'taskkill.exe /F /IM spora-wincore.exe'
    Pop $0
    Sleep 500

    SetOutPath "$INSTDIR"
    File "${WINUI_EXE}"
    File "${WINCORE_EXE}"
    File "${WINTUN_DLL}"
    File "${ICON_FILE}"

    ; Install (or refresh) the spora-wincore Windows service. The binPath must
    ; embed backslash-escaped quotes around the exe path so sc.exe stores an
    ; ImagePath whose path survives the space in "Program Files"; NSIS `\$\"`
    ; emits a literal `\"` on the command line.
    nsExec::ExecToLog 'sc.exe create Spora binPath= "\$\"$INSTDIR\spora-wincore.exe\$\" --service" start= auto DisplayName= "Spora VPN Service"'
    Pop $0
    ${If} $0 != 0
        ; Service already exists (upgrade) — refresh its configuration
        nsExec::ExecToLog 'sc.exe config Spora binPath= "\$\"$INSTDIR\spora-wincore.exe\$\" --service" start= auto'
        Pop $0
    ${EndIf}
    nsExec::ExecToLog 'sc.exe description Spora "Runs Spora VPN tunnels and the network configuration they need."'
    Pop $0
    ; Restart on crash (5s, 5s, 30s); reset the failure counter after a day
    nsExec::ExecToLog 'sc.exe failure Spora reset= 86400 actions= restart/5000/restart/5000/restart/30000'
    Pop $0
    ; Apply the restart actions to non-crash failures too (a non-zero exit,
    ; e.g. the pipe was unavailable at startup), so a failed start self-heals.
    nsExec::ExecToLog 'sc.exe failureflag Spora 1'
    Pop $0
    nsExec::ExecToLog 'sc.exe start Spora'
    Pop $0

    ; Verify the service actually exists. `sc create`/`config` can fail (e.g.
    ; 1072 ERROR_SERVICE_MARKED_FOR_DELETE if a prior uninstall left the
    ; service pending deletion because something held a handle to it). Since
    ; every sc exit code above is discarded, detect that here so the install
    ; doesn't finish with a green success page but no working service.
    nsExec::ExecToLog 'sc.exe query Spora'
    Pop $0
    ${If} $0 != 0
        MessageBox MB_ICONEXCLAMATION|MB_OK \
            "The Spora background service could not be installed. A previous \
version may be pending removal. Please reboot and run this installer again."
    ${EndIf}

    ; Start Menu shortcuts
    CreateDirectory "$SMPROGRAMS\Spora"
    CreateShortcut "$SMPROGRAMS\Spora\Spora.lnk" "$INSTDIR\spora-winui.exe" "" "$INSTDIR\spora.ico"
    CreateShortcut "$SMPROGRAMS\Spora\Uninstall.lnk" "$INSTDIR\uninstall.exe"

    ; Write uninstaller
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Add/Remove Programs registry
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "DisplayName" "Spora"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "DisplayVersion" "${VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "UninstallString" '"$INSTDIR\uninstall.exe"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "InstallLocation" "$INSTDIR"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "DisplayIcon" '"$INSTDIR\spora.ico"'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "Publisher" "Spora"
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "NoModify" 1
    WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora" \
        "NoRepair" 1

    ; Register spora: URI scheme
    WriteRegStr HKLM "Software\Classes\spora" "" "URL:Spora"
    WriteRegStr HKLM "Software\Classes\spora" "URL Protocol" ""
    WriteRegStr HKLM "Software\Classes\spora\DefaultIcon" "" "$INSTDIR\spora.ico"
    WriteRegStr HKLM "Software\Classes\spora\shell\open\command" "" \
        '"$INSTDIR\spora-winui.exe" "%1"'

    ; Remember install dir
    WriteRegStr HKLM "Software\Spora" "InstallDir" "$INSTDIR"
SectionEnd

Section "Desktop Shortcut" SecDesktop
    CreateShortcut "$DESKTOP\Spora.lnk" "$INSTDIR\spora-winui.exe" "" "$INSTDIR\spora.ico"
SectionEnd

; ── Section Descriptions ─────────────────────────────────────────────

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
    !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "Install Spora VPN tunnel."
    !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Create a desktop shortcut."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ── Uninstaller ──────────────────────────────────────────────────────

Section "Uninstall"
    ; Kill the UI, then clear failure actions before stopping so nothing
    ; auto-restarts wincore between stop and delete, then remove the service.
    nsExec::ExecToLog 'taskkill.exe /F /IM spora-winui.exe'
    Pop $0
    nsExec::ExecToLog 'sc.exe failure Spora reset= 0 actions= ""'
    Pop $0
    nsExec::ExecToLog 'sc.exe stop Spora'
    Pop $0
    ; Wait for the service to report STOPPED (up to ~20s) before deleting its
    ; files, so a slow tunnel teardown doesn't leave spora-wincore.exe locked.
    StrCpy $1 0
    un_wincore_wait:
        nsExec::ExecToStack 'cmd /c sc query Spora | findstr STOPPED'
        Pop $0
        Pop $2
        ${If} $0 == 0
            Goto un_wincore_stopped
        ${EndIf}
        IntOp $1 $1 + 1
        ${If} $1 >= 20
            Goto un_wincore_stopped
        ${EndIf}
        Sleep 1000
        Goto un_wincore_wait
    un_wincore_stopped:
    nsExec::ExecToLog 'sc.exe delete Spora'
    Pop $0
    ; Kill any straggler and give the OS a moment to release the file handle.
    nsExec::ExecToLog 'taskkill.exe /F /IM spora-wincore.exe'
    Pop $0
    Sleep 1000

    ; Remove files
    Delete "$INSTDIR\spora-winui.exe"
    Delete "$INSTDIR\spora-wincore.exe"
    Delete "$INSTDIR\wintun.dll"
    Delete "$INSTDIR\spora.ico"
    Delete "$INSTDIR\uninstall.exe"
    RMDir "$INSTDIR"

    ; Remove shortcuts
    Delete "$SMPROGRAMS\Spora\Spora.lnk"
    Delete "$SMPROGRAMS\Spora\Uninstall.lnk"
    RMDir "$SMPROGRAMS\Spora"
    Delete "$DESKTOP\Spora.lnk"

    ; Remove registry keys
    DeleteRegKey HKLM "Software\Classes\spora"
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora"
    DeleteRegKey HKLM "Software\Spora"
SectionEnd
