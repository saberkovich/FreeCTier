//! Service Control Manager integration: dispatch entry, status reporting and
//! stop signalling for the named-pipe accept loop.

use crate::{log, pipe, Ctl, SERVICE_NAME};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

define_windows_service!(ffi_service_main, service_main);

/// Called by the Service Control Manager when the service starts.
pub fn dispatch() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn service_main(arguments: Vec<OsString>) {
    let _ = arguments;
    if let Err(error) = run_as_service() {
        log(&format!("service run failed: {error:#}"));
    }
}

fn report(
    handler: &service_control_handler::ServiceStatusHandle,
    state: ServiceState,
    wait_hint: Duration,
) -> Result<()> {
    handler
        .set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint,
            process_id: None,
        })
        .context("Cannot report service status")
}

fn run_as_service() -> Result<()> {
    let ctl = Arc::new(Ctl::new()?);
    let handler_ctl = Arc::clone(&ctl);
    let handler = service_control_handler::register(SERVICE_NAME, move |control| match control {
        // Cancelling the pipe I/O wakes both the accept loop and the active
        // session; the stop flag keeps them from accepting work again.
        ServiceControl::Stop => {
            let client = *handler_ctl.client.lock().unwrap();
            handler_ctl.request_stop(client);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .context("Cannot register service control handler")?;
    report(&handler, ServiceState::StartPending, Duration::from_secs(5))?;
    report(&handler, ServiceState::Running, Duration::from_secs(5))?;
    log("service running");
    let user = crate::service_user().unwrap_or_default();
    pipe::serve(&ctl, &user);
    report(&handler, ServiceState::StopPending, Duration::from_secs(5))?;
    report(&handler, ServiceState::Stopped, Duration::from_secs(0))?;
    let _ = Ordering::SeqCst;
    Ok(())
}
