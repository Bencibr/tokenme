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
!macro NSIS_HOOK_PREUNINSTALL
  ; The POSTINSTALL hook pinned the toast identity here; the template knows
  ; nothing about this key and would leave it behind. Whole-key delete: the
  ; key carries only values this product wrote.
  DeleteRegKey SHCTX "Software\Classes\AppUserModelId\${BUNDLEID}"
!macroend

!macro NSIS_HOOK_POSTINSTALL
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
