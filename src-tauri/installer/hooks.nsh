; Uninstall hook.
;
; Headroom's footprint is written *after* install and mostly outside the file
; list NSIS tracks: the managed Python runtime and model caches under
; %LOCALAPPDATA%\Headroom (which is also $INSTDIR for a currentUser install, so
; the template's non-recursive `RMDir "$INSTDIR"` silently fails), ~\.headroom,
; the autostart Run key, and our edits to Claude/Codex configs, hooks, MCP
; registrations and shell rc files. The template only deletes what it installed,
; and its "delete app data" checkbox targets $LOCALAPPDATA\<bundle id>, a
; directory we never write. Net effect before this hook: uninstalling left the
; whole runtime on disk and every agent still routed at a dead proxy, so a
; reinstall came back with all of the previous state.
;
; So let the app tear itself down first, while its exe is still there.
; `--uninstall` runs the same cleanup as the in-app uninstall and exits before
; any window or the proxy comes up.
;
; Skipped when $UpdateMode is 1: the updater passes /UPDATE to the installer and
; the installer passes it on to the old uninstaller, and an update must keep
; user data.
;
; Also skipped when the uninstaller runs from $INSTDIR itself. Running a newer
; installer by hand shows the template's "already installed" page with
; "Uninstall before installing" preselected, which runs the old uninstaller with
; `_?=$INSTDIR` and no /UPDATE, then installs straight away: the cleanup wiped
; the savings history and everything under ~\.headroom on a plain upgrade
; (2026-10-07). `_?=` is also the only thing that stops NSIS from running the
; uninstaller from a %TEMP% copy, so a real uninstall (Windows Settings, a
; double-clicked uninstall.exe, the in-app uninstall's `/S` launch) never runs
; from $INSTDIR and still gets the full cleanup.
!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
  ${AndIf} $EXEDIR != $INSTDIR
    ; Stop the running instance first, or it rewrites the config dir we are
    ; about to delete. Deliberately not the CheckIfAppIsRunning macro: the
    ; template inserts it a few lines below, in this same section, and its
    ; labels would collide. That insertion is what reports a failed kill.
    nsis_tauri_utils::KillProcessCurrentUser "${MAINBINARYNAME}.exe"
    Pop $0
    Sleep 500
    ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall'
    ClearErrors
  ${EndIf}
!macroend
