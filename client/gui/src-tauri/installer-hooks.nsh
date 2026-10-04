# FreeC Tier NSIS installer hooks: stop the old service before its binary is
; replaced (a running service keeps freec-service.exe locked), register the
; service on install and remove it before the files are deleted. The
; installer already runs elevated.
!macro NSIS_HOOK_PREINSTALL
  ; Straight-line stop with no loops or exit-code parsing: every command
  ; here is bounded, so the installer can never stall on this hook. A hard
  ; kill is safe — the service is a stateless packet pump.
  DetailPrint "Остановка службы FreeC Tier..."
  nsExec::Exec 'sc.exe stop FreeCTierService'
  Pop $0
  nsExec::Exec 'taskkill.exe /F /IM freec-service.exe'
  Pop $0
  Sleep 1000
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\freec-service.exe" --install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog '"$INSTDIR\freec-service.exe" --uninstall'
!macroend
