; Hooks into Tauri's NSIS installer (bundle.windows.nsis.installerHooks in tauri.conf.json).

; Up to v0.1.2 the product was named "LoL Death Replay", so it installed to
; %LOCALAPPDATA%\LoL Death Replay. The installer knows an app by that name, so after the rename it
; doesn't see the old install as "this app". Remove it here, keeping "Start with Windows" if it was on.
; Settings and clips are untouched: they live under the bundle identifier and in Videos.
!define OLD_PRODUCTNAME "LoL Death Replay"

!macro NSIS_HOOK_POSTINSTALL
  ReadRegStr $R0 HKCU "Software\${MANUFACTURER}\${OLD_PRODUCTNAME}" "" ; the old install folder
  ${If} $R0 != ""
  ${AndIf} ${FileExists} "$R0\uninstall.exe"
    ; Autostart is a Run value named after the product, holding the exe path (see tauri-plugin-autostart).
    ReadRegStr $R1 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${OLD_PRODUCTNAME}"
    ${If} $R1 != ""
      WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}" "$INSTDIR\${MAINBINARYNAME}.exe "
    ${EndIf}
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${OLD_PRODUCTNAME}"

    ; Its own uninstaller removes the files, shortcuts, uninstall entry and old Run value.
    ; _?= runs it in place (no temp copy), so it can't delete itself or the folder: do that after.
    ExecWait '"$R0\uninstall.exe" /S _?=$R0'
    Delete "$R0\uninstall.exe"
    RMDir "$R0"
    DeleteRegKey HKCU "Software\${MANUFACTURER}\${OLD_PRODUCTNAME}"
  ${EndIf}
!macroend
