; Spora NSIS Installer Script
; Expects defines passed from build script:
;   WINUI_EXE  - path to spora-winui.exe
;   WINCORE_EXE - path to spora-wincore.exe
;   WINTUN_DLL - path to wintun.dll
;   ICON_FILE   - path to spora.ico
;   LICENSES_DIR - directory of the .txt licence files installed under
;                 $INSTDIR\licenses (Spora's own, Wintun's, IBM Plex's)
;   OUTFILE_PATH - output installer exe path
;   VERSION     - release version (the workspace version, e.g. 0.2.1)
;   VERSION_NUMERIC - the same as four numeric fields (0.2.1.0) for
;                 the installer's own version resource

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"

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

; The installer's own file properties (Explorer's Details tab).
VIProductVersion "${VERSION_NUMERIC}"
VIAddVersionKey "ProductName" "Spora"
VIAddVersionKey "FileDescription" "Spora Setup"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Copyright (C) 2026 The Spora authors. GPL-3.0."

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

; Leaving the confirm page asks whether to delete Spora's data too.
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.AskDeleteData
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ── Install Sections ─────────────────────────────────────────────────

; Upgrades and reinstalls install over what is there: they stop the service,
; replace the files, refresh the service's configuration and write a new
; uninstaller. They never run the old uninstaller, and nothing here deletes
; Spora's data (see "Spora's data" below). A future installer that does run
; the old uninstaller must run it silently and without /DELETEDATA.

Section "Spora (required)" SecMain
    SectionIn RO

    ; Kill any running UI so its exe can be replaced (all install types).
    nsExec::ExecToLog 'taskkill.exe /F /IM spora-winui.exe'
    Pop $0

    ; If the service exists, this is an upgrade. Clear its restart-on-failure
    ; actions FIRST so the taskkill fallback below can't trip an SCM
    ; auto-restart mid-extraction, then stop it and wait for it to actually
    ; report STOPPED before we overwrite its files. (On a fresh install sc
    ; query returns 1060 and we skip this.)
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

    ; Belt-and-suspenders: kill any lingering wincore (a service that didn't
    ; stop within the wait, or a console-mode one) so its exe/dll aren't
    ; locked when we extract.
    nsExec::ExecToLog 'taskkill.exe /F /IM spora-wincore.exe'
    Pop $0
    Sleep 500

    SetOutPath "$INSTDIR"
    File "${WINUI_EXE}"
    File "${WINCORE_EXE}"
    File "${WINTUN_DLL}"
    File "${ICON_FILE}"

    ; Licences: Spora's (GPL-3.0 + NOTICE), and those of what it bundles
    ; (wintun.dll, the IBM Plex fonts compiled into the UI). Back to $INSTDIR
    ; afterwards: the shortcuts below take $OUTDIR as their working directory.
    SetOutPath "$INSTDIR\licenses"
    File "${LICENSES_DIR}\*.txt"
    SetOutPath "$INSTDIR"

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

; Spora's data stays unless the uninstall is asked to delete it: an
; interactive uninstall asks (default Yes) when its confirm page is left; a
; silent one (/S) keeps it unless run with /DELETEDATA, which also skips
; the question in an interactive one:
;
;   uninstall.exe /S               keep the data
;   uninstall.exe /S /DELETEDATA   delete it
Var DeleteData

Function un.onInit
    StrCpy $DeleteData 0
    ${GetParameters} $R0
    ClearErrors
    ${GetOptions} $R0 "/DELETEDATA" $R1
    ${IfNot} ${Errors}
        StrCpy $DeleteData 1
    ${EndIf}
FunctionEnd

Function un.AskDeleteData
    ${If} $DeleteData == 1
        Return
    ${EndIf}
    MessageBox MB_YESNOCANCEL|MB_ICONQUESTION|MB_DEFBUTTON1 \
        "Also delete Spora's data from this computer?$\r$\n$\r$\n\
Yes deletes, for every Windows user of this computer:$\r$\n\
- the device identity: share links you gave out stop working for good;$\r$\n\
- saved connections, relay settings and telemetry settings;$\r$\n\
- the connection logs of your shares, which record what was reached \
through them and when. You may need them to answer an abuse report about \
traffic that went through your connection;$\r$\n\
- diagnostic logs, connection records, crash reports and the app's own \
logs.$\r$\n$\r$\n\
No keeps all of it for a later reinstall. Reports you saved yourself are \
not touched." \
        /SD IDNO IDYES ask_delete IDNO ask_keep
    ; Cancel: back to the confirm page.
    Abort
    ask_delete:
        StrCpy $DeleteData 1
        Return
    ask_keep:
        StrCpy $DeleteData 0
FunctionEnd

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
    RMDir /r "$INSTDIR\licenses"
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
    ; The UI's toast registration (src/toast.rs), the running user's only:
    ; other users' registry hives are not loaded. The UI makes it again.
    SetRegView 64
    DeleteRegKey HKCU "Software\Classes\AppUserModelId\Spora.Client"
    SetRegView default

    ; Last, with the service and the UI gone, nothing holds their files.
    ${If} $DeleteData == 1
        Call un.DeleteData
    ${Else}
        DetailPrint "Kept Spora's data (%ProgramData%\Spora and each user's AppData)."
    ${EndIf}
SectionEnd

; ── Spora's data ─────────────────────────────────────────────────────
;
; What Spora keeps, all of it deleted by un.DeleteData:
; - the service's, for every Windows user: %ProgramData%\Spora. Each user's
;   device identity, relay and telemetry settings, connection logs,
;   connection records, diagnostic log and crash files under
;   users\<SID>\ (directly under Spora\ in earlier versions), and the
;   service log spora-wincore.log*. Nothing else writes there.
; - the UI's, per Windows user: %APPDATA%\spora\config\connections.json*
;   (the saved connections, the shares' secrets among them) and
;   %LOCALAPPDATA%\Spora\spora-winui.log* (spora-wincore.log* too, from
;   versions before the service). Only those files: the spora CLI keeps its
;   own share (%APPDATA%\spora\share.bin) and state (%LOCALAPPDATA%\spora\)
;   in the same folders, and is not this installer's to delete. A folder
;   goes only when nothing else is left in it.
; Diagnostic reports the user saved (Downloads\spora-report-*.zip) are theirs.
;
; The uninstaller runs elevated, as whoever approved the elevation: its
; $APPDATA and $LOCALAPPDATA are that user's, wherever they are redirected.
; Every other profile on the machine (ProfileList) is cleaned at the default
; AppData locations under its profile folder; a roaming profile's server
; copy and AppData redirected elsewhere are out of reach.

!define PROFILE_LIST "SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList"

Function un.DeleteData
    DetailPrint "Deleting Spora's data..."
    ReadEnvStr $R0 "ProgramData"
    ${If} $R0 == ""
        SetShellVarContext all
        StrCpy $R0 "$APPDATA"
        SetShellVarContext current
    ${EndIf}
    ${If} $R0 != ""
        RMDir /r "$R0\Spora"
        ${If} ${FileExists} "$R0\Spora\*.*"
            DetailPrint "Could not delete all of $R0\Spora: some files are in use."
        ${EndIf}
    ${EndIf}

    Push "$LOCALAPPDATA"
    Push "$APPDATA"
    Call un.DeleteUserData

    SetRegView 64
    StrCpy $R1 0
    ${Do}
        ClearErrors
        EnumRegKey $R2 HKLM "${PROFILE_LIST}" $R1
        ${If} ${Errors}
        ${OrIf} $R2 == ""
            ${Break}
        ${EndIf}
        IntOp $R1 $R1 + 1
        ReadRegStr $R3 HKLM "${PROFILE_LIST}\$R2" "ProfileImagePath"
        ${If} $R3 != ""
            ExpandEnvStrings $R3 "$R3"
            Push "$R3\AppData\Local"
            Push "$R3\AppData\Roaming"
            Call un.DeleteUserData
        ${EndIf}
    ${Loop}
    SetRegView default
FunctionEnd

; One user's UI files. Stack: roaming AppData on top, local AppData below.
Function un.DeleteUserData
    Pop $R8
    Pop $R9
    ${If} $R8 != ""
        Delete "$R8\spora\config\connections.json*"
        RMDir "$R8\spora\config"
        RMDir "$R8\spora"
    ${EndIf}
    ${If} $R9 != ""
        Delete "$R9\Spora\spora-winui.log*"
        Delete "$R9\Spora\spora-wincore.log*"
        RMDir "$R9\Spora"
    ${EndIf}
FunctionEnd
