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
!define TOKENME_ARCHIVE_SCRIPT "${__FILEDIR__}\..\..\..\scripts\archive-windows-build.mjs"

!macro TokenMeCrashDumps MODE SCRIPT
  ${If} ${FileExists} "${SCRIPT}"
    nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "${SCRIPT}" -Mode ${MODE} -InstallDirectory "$INSTDIR"'
    Pop $0
    Pop $1
    ${If} $0 != 0
      ; A non-admin per-user install still installs the app. Startup reports
      ; that full dumps need an administrator repair instead of claiming success.
      DetailPrint "TokenMe crash dump setup ${MODE} unavailable: $1"
    ${EndIf}
  ${EndIf}
!macroend

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

!macro NSIS_HOOK_PREUNINSTALL
  ; Keep the helper until POSTUNINSTALL: the app-running check may cancel an
  ; uninstall after this hook, in which case PATH must stay intact.
  ${If} ${FileExists} "$INSTDIR\bin\windows-cli-path.ps1"
    InitPluginsDir
    CopyFiles /SILENT "$INSTDIR\bin\windows-cli-path.ps1" "$PLUGINSDIR\windows-cli-path.ps1"
  ${EndIf}
  ${If} ${FileExists} "$INSTDIR\bin\windows-crash-dumps.ps1"
    InitPluginsDir
    CopyFiles /SILENT "$INSTDIR\bin\windows-crash-dumps.ps1" "$PLUGINSDIR\windows-crash-dumps.ps1"
  ${EndIf}
  ; The POSTINSTALL hook pinned the toast identity here; the template knows
  ; nothing about this key and would leave it behind. Whole-key delete: the
  ; key carries only values this product wrote.
  DeleteRegKey SHCTX "Software\Classes\AppUserModelId\${BUNDLEID}"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro TokenMeCliPath Add "$INSTDIR\bin\windows-cli-path.ps1"
  !insertmacro TokenMeCrashDumps Enable "$INSTDIR\bin\windows-crash-dumps.ps1"
  ; Archive the exact executable after Tauri's bundle-type patch/signing.
  !finalize 'node "${TOKENME_ARCHIVE_SCRIPT}" --exe "${MAINBINARYSRCPATH}"' = 0
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
  !insertmacro TokenMeCrashDumps Disable "$PLUGINSDIR\windows-crash-dumps.ps1"
!macroend
