# FreeC Tier NSIS installer hooks: stop the old service before its binary is
; replaced (a running service keeps freec-service.exe locked), register the
; service on install and remove it before the files are deleted. The
; installer already runs elevated.
!macro NSIS_HOOK_PREINSTALL
  DetailPrint "Остановка службы FreeC Tier..."
  nsExec::ExecToLog 'sc.exe stop FreeCTierService'
  Pop $0
  StrCpy $R0 0
freec_wait_stop:
  nsExec::Exec 'tasklist.exe /FI "IMAGENAME eq freec-service.exe" /FO csv /NH'
  Pop $0 ; exit code: 0 = still running, 1 = gone
  IntCmp $0 0 freec_running freec_stopped freec_stopped
freec_running:
  IntCmp $R0 20 freec_stopped freec_wait_more freec_stopped
freec_wait_more:
  IntOp $R0 $R0 + 1
  Sleep 1000
  Goto freec_wait_stop
freec_stopped:
  ; A wedged service ignores Stop — force kill so the file is unlocked.
  nsExec::Exec 'taskkill.exe /F /IM freec-service.exe'
  Pop $0
  Sleep 500
!macroend

!macro NSIS_HOOK_POSTINSTALL
  nsExec::ExecToLog '"$INSTDIR\freec-service.exe" --install'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::ExecToLog '"$INSTDIR\freec-service.exe" --uninstall'
!macroend
