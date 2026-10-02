; FreeC Tier NSIS installer hooks: register the Wintun service on install and
; remove it before the files are deleted. The installer already runs elevated.
!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\freec-service.exe" --install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog '"$INSTDIR\freec-service.exe" --uninstall'
!macroend
