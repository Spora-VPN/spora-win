; Spora NSIS Installer Script
; Expects defines passed from build script:
;   WINUI_EXE  - path to spora-winui.exe
;   WINCORE_EXE - path to spora-wincore.exe
;   WINTUN_DLL - path to wintun.dll
;   ICON_FILE   - path to spora.ico
;   OUTFILE_PATH - output installer exe path

!include "MUI2.nsh"

; ── General ──────────────────────────────────────────────────────────

Name "Spora"
OutFile "${OUTFILE_PATH}"
InstallDir "$PROGRAMFILES64\Spora"
InstallDirRegKey HKLM "Software\Spora" "InstallDir"
RequestExecutionLevel admin

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

    SetOutPath "$INSTDIR"
    File "${WINUI_EXE}"
    File "${WINCORE_EXE}"
    File "${WINTUN_DLL}"
    File "${ICON_FILE}"

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
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\Spora"
    DeleteRegKey HKLM "Software\Spora"
SectionEnd
