; NSIS installer hooks (tauri.conf.json bundle.windows.nsis.installerHooks).
;
; The toast banner names the app after the Start Menu shortcut's file name, and
; a shortcut from the lowercase era keeps that spelling forever: on a
; case-insensitive filesystem CreateShortcut writes into the existing directory
; entry instead of renaming it, and update mode skips shortcut creation
; entirely (installer.nsi's CreateOrUpdateStartMenuShortcut returns early).
; This hook runs in every install, update or not: delete every spelling this
; product has used — mainBinaryName has been tokenme-bar, tokenme and TokenMe —
; then recreate the canonical one with the AUMID the notify worker posts under.
; Run outside NSIS's fixed string buffer so long PATH values are never truncated.
; MultiUser selects the same scope for environment and product registration.
!macro TokenMeCliPath MODE SCRIPT
  ${If} ${FileExists} "${SCRIPT}"
    StrCpy $0 "User"
    !if "${INSTALLMODE}" == "both"
      ${If} $MultiUser.InstallMode == "AllUsers"
        StrCpy $0 "Machine"
      ${EndIf}
    !else if "${INSTALLMODE}" == "perMachine"
      StrCpy $0 "Machine"
    !endif
    nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "${SCRIPT}" -Mode ${MODE} -Scope $0 -Directory "$INSTDIR\bin"'
    Pop $0
    Pop $1
    ${If} $0 != 0
      DetailPrint "TokenMe CLI PATH ${MODE} failed: $1"
      SetErrorLevel 1
    ${EndIf}
  ${EndIf}
!macroend

; An interrupted or older uninstall can leave the Add/Remove Programs entry
; behind after its uninstaller.exe has gone away. Tauri's default maintenance
; page tries that path before any installer hook runs, so the custom NSIS
; template calls this repair macro while that page is being prepared.
!macro TokenMeRepairStaleUninstall
  ReadRegStr $R0 SHCTX "${MANUPRODUCTKEY}" ""
  ${If} "$R0" == ""
    ReadRegStr $R0 SHCTX "${UNINSTKEY}" "InstallLocation"
    ${If} "$R0" != ""
      StrCpy $R1 $R0 1
      ${If} "$R1" == "$\""
        StrLen $R1 $R0
        IntOp $R1 $R1 - 2
        StrCpy $R0 $R0 $R1 1
      ${EndIf}
    ${EndIf}
  ${EndIf}
  ${If} "$R0" == ""
    Return
  ${EndIf}
  ${If} ${FileExists} "$R0\uninstall.exe"
    Return
  ${EndIf}
  DetailPrint "Removing stale TokenMe uninstall registration"
  DeleteRegKey SHCTX "${UNINSTKEY}"
  DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
!macroend

; Remove a duplicate registration for the same physical install after a
; successful uninstall. This covers the opposite HKCU/HKLM view left by old
; installers without deleting a registration for another install directory.
!macro TokenMeDeleteMatchingUninstall ROOT
  ReadRegStr $R0 ${ROOT} "${MANUPRODUCTKEY}" ""
  ${If} "$R0" == "$INSTDIR"
    DeleteRegKey ${ROOT} "${UNINSTKEY}"
    DeleteRegKey ${ROOT} "${MANUPRODUCTKEY}"
  ${Else}
    ReadRegStr $R0 ${ROOT} "${UNINSTKEY}" "InstallLocation"
    ${If} "$R0" != ""
      StrCpy $R1 $R0 1
      ${If} "$R1" == "$\""
        StrLen $R1 $R0
        IntOp $R1 $R1 - 2
        StrCpy $R0 $R0 $R1 1
      ${EndIf}
      ${If} "$R0" == "$INSTDIR"
        DeleteRegKey ${ROOT} "${UNINSTKEY}"
        DeleteRegKey ${ROOT} "${MANUPRODUCTKEY}"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Keep the helper until POSTUNINSTALL: the app-running check may cancel an
  ; uninstall after this hook, in which case PATH must stay intact.
  ${If} ${FileExists} "$INSTDIR\bin\windows-cli-path.ps1"
    InitPluginsDir
    CopyFiles /SILENT "$INSTDIR\bin\windows-cli-path.ps1" "$PLUGINSDIR\windows-cli-path.ps1"
  ${EndIf}
  ; The POSTINSTALL hook pinned the toast identity here; the template knows
  ; nothing about this key and would leave it behind. Whole-key delete: the
  ; key carries only values this product wrote.
  DeleteRegKey SHCTX "Software\Classes\AppUserModelId\${BUNDLEID}"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro TokenMeCliPath Add "$INSTDIR\bin\windows-cli-path.ps1"
  ; Firewall: discovery answers arrive as inbound UDP 5353 and the LAN sync
  ; listener takes inbound TCP, but Windows never shows its allow prompt for
  ; silent installs — the default-deny profile drops both and a secondary
  ; scans an empty LAN forever. The installer holds an elevated token
  ; (MultiUser highestAvailable), so register allow rules for this exact
  ; binary; the stale lowercase-era rules name paths this exe outlived.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="TokenMe (mDNS discovery)"'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall add rule name="TokenMe (mDNS discovery)" dir=in action=allow program="$INSTDIR\${MAINBINARYNAME}.exe" protocol=UDP localport=5353 profile=any enable=yes'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall add rule name="TokenMe (LAN sync)" dir=in action=allow program="$INSTDIR\${MAINBINARYNAME}.exe" protocol=TCP profile=any enable=yes'
  Pop $0
  ; Pin the toast identity: the notification banner resolves the app name
  ; through shell caches that a renamed shortcut does not invalidate, so the
  ; registration wins where the shortcut spelling loses.
  WriteRegStr SHCTX "Software\Classes\AppUserModelId\${BUNDLEID}" "DisplayName" "${PRODUCTNAME}"
  ; A toast identity without an IconUri renders the generic blank badge — the
  ; installer ships the icon next to the exe (tauri.conf.json resources) so the
  ; registration can point at it.
  WriteRegStr SHCTX "Software\Classes\AppUserModelId\${BUNDLEID}" "IconUri" "$INSTDIR\icon.ico"
  Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  Delete "$SMPROGRAMS\tokenme-bar.lnk"
  Delete "$SMPROGRAMS\tokenme.lnk"
  CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  !insertmacro SetLnkAppUserModelId "$SMPROGRAMS\${PRODUCTNAME}.lnk"
  ; The desktop shortcut is only rewritten when one is already there: the user
  ; may have deleted it on purpose, and no toast name rides on it.
  ${If} ${FileExists} "$DESKTOP\${PRODUCTNAME}.lnk"
  ${OrIf} ${FileExists} "$DESKTOP\tokenme-bar.lnk"
    Delete "$DESKTOP\${PRODUCTNAME}.lnk"
    Delete "$DESKTOP\tokenme-bar.lnk"
    Delete "$DESKTOP\tokenme.lnk"
    CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    !insertmacro SetLnkAppUserModelId "$DESKTOP\${PRODUCTNAME}.lnk"
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro TokenMeCliPath Remove "$PLUGINSDIR\windows-cli-path.ps1"
  ; A per-user uninstaller may run unelevated and netsh will fail there; the
  ; rules then survive until the next install overwrites them.
  nsExec::Exec 'netsh advfirewall firewall delete rule name="TokenMe (mDNS discovery)"'
  Pop $0
  nsExec::Exec 'netsh advfirewall firewall delete rule name="TokenMe (LAN sync)"'
  Pop $0
  !insertmacro TokenMeDeleteMatchingUninstall HKLM
  !insertmacro TokenMeDeleteMatchingUninstall HKCU
!macroend
