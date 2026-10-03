//! One-time elevated installation: registers the service in the SCM, records
//! the installing user's SID for the pipe DACL, and starts it. Uninstall is
//! the mirror image. Both run via ShellExecuteW "runas" from the UI.

use crate::{log, REGISTRY_KEY, SERVICE_DESCRIPTION, SERVICE_DISPLAY, SERVICE_NAME};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, LocalFree, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_ACCESS_MASK, TOKEN_USER};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows_service::service::{
    ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub fn install() -> Result<()> {
    let exe = std::env::current_exe().context("Cannot locate service executable")?;
    let sid = current_user_sid()?;
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    let (key, _) = hklm
        .create_subkey(REGISTRY_KEY)
        .context("Cannot write service registry key")?;
    key.set_value("ServiceUser", &sid)
        .context("Cannot record service user")?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("Cannot open service database")?;
    // Upgrades replace the registered binary: stop and delete an existing
    // installation first, otherwise create_service fails with
    // ERROR_SERVICE_EXISTS and the running old build keeps serving.
    if let Ok(existing) = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    ) {
        if existing
            .query_status()
            .map(|s| s.current_state == ServiceState::Running)
            .unwrap_or(false)
        {
            let _ = existing.stop();
            for _ in 0..30 {
                if existing
                    .query_status()
                    .map(|s| s.current_state != ServiceState::Running)
                    .unwrap_or(true)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        existing
            .delete()
            .context("Cannot replace the old service")?;
        // SCM marks the entry for deletion; it frees once all handles close.
        std::thread::sleep(Duration::from_millis(500));
    }
    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec![OsString::from("--service")],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::ALL_ACCESS)
        .context("Cannot create the service")?;
    let _ = service.set_description(SERVICE_DESCRIPTION);
    service
        .start(&[] as &[std::ffi::OsString])
        .context("Cannot start the service")?;
    log("service installed and started");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .context("Service is not installed")?;
    if service.query_status()?.current_state == ServiceState::Running {
        service.stop()?;
        for _ in 0..30 {
            if service.query_status()?.current_state != ServiceState::Running {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    service.delete().context("Cannot delete the service")?;
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    let _ = hklm.delete_subkey(REGISTRY_KEY);
    log("service uninstalled");
    Ok(())
}

/// SID of the caller. The elevated process keeps the invoking user's account
/// (UAC elevation does not switch users), so this is the user to admit into
/// the pipe DACL.
pub fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = Default::default();
        windows::Win32::System::Threading::OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ACCESS_MASK(0x0008),
            &mut token,
        )
        .context("OpenProcessToken failed")?;
        let mut needed = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        let mut buffer = vec![0u8; needed as usize];
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
        .context("GetTokenInformation failed")?;
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut string = windows::core::PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut string)
            .context("ConvertSidToStringSidW failed")?;
        let result = string.to_string().unwrap_or_default();
        if !result.is_empty() {
            let _ = LocalFree(Some(HLOCAL(string.as_ptr() as *mut std::ffi::c_void)));
        }
        let _ = CloseHandle(token);
        Ok(result)
    }
}
