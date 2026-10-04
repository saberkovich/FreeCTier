//! Elevated Windows service that owns the Wintun adapters for FreeC Tier.
//!
//! Trust model: the service performs no traffic validation — the UI process
//! (Steam engine) keeps routing and anti-spoof checks because packet
//! validation needs the Steam sender identity. The service only bridges the
//! named pipe to Wintun sessions; the pipe DACL restricts who may connect.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::HANDLE;

mod adapters;
mod install;
mod pipe;
mod scm;

pub const SERVICE_NAME: &str = "FreeCTierService";
const SERVICE_DISPLAY: &str = "FreeC Tier Service";
const SERVICE_DESCRIPTION: &str = "Manages virtual Wintun network adapters for FreeC Tier.";
const REGISTRY_KEY: &str = "SOFTWARE\\FreeC Tier";

// The struct only holds Win32 handles; every use goes through thread-safe
// Win32 calls (SetEvent, CancelIoEx), so sharing across threads is sound.
unsafe impl Send for Ctl {}
unsafe impl Sync for Ctl {}

pub struct Ctl {
    /// Shared with adapter reader threads, which outlive individual sessions.
    pub stop_flag: Arc<AtomicBool>,
    stop_event: HANDLE,
    /// Raw handle value of the connected client pipe, if any; used to cancel
    /// in-flight I/O when the service is asked to stop.
    client: Mutex<Option<isize>>,
}

impl Ctl {
    fn new() -> anyhow::Result<Self> {
        use windows::Win32::System::Threading::CreateEventW;
        Ok(Self {
            stop_flag: Arc::new(AtomicBool::new(false)),
            stop_event: unsafe { CreateEventW(None, true, false, None)? },
            client: Mutex::new(None),
        })
    }

    fn request_stop(&self, client: Option<isize>) {
        use windows::Win32::System::Threading::SetEvent;
        use windows::Win32::System::IO::CancelIoEx;
        self.stop_flag.store(true, Ordering::SeqCst);
        unsafe {
            let _ = SetEvent(self.stop_event);
            if let Some(raw) = client {
                let _ = CancelIoEx(HANDLE(raw as *mut _), None);
            }
        }
    }

    fn is_stopping(&self) -> bool {
        self.stop_flag.load(Ordering::SeqCst)
    }
}

/// UTC timestamp without pulling in a date library.
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn log(message: &str) {
    let program_data = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let log = program_data.join("FreeC Tier").join("service.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        use std::io::Write;
        let _ = writeln!(file, "{}: {message}", utc_stamp());
    }
    eprintln!("{message}");
}

/// SID of the user the service was installed for, from the machine registry.
fn service_user() -> anyhow::Result<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    let key = winreg::RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(REGISTRY_KEY)?;
    Ok(key.get_value::<String, _>("ServiceUser")?)
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().collect();
    let mode = args
        .get(1)
        .and_then(|a| a.to_str())
        .unwrap_or("--help")
        .to_owned();
    let result = match mode.as_str() {
        "--service" => scm::dispatch(),
        "--console" => run_console(),
        "--install" => install::install(),
        "--uninstall" => install::uninstall(),
        other => {
            eprintln!("Usage: freec-service [--service|--console|--install|--uninstall]");
            if other != "--help" {
                Err(anyhow::anyhow!("Unknown mode: {other}"))
            } else {
                Ok(())
            }
        }
    };
    if let Err(error) = result {
        log(&format!("fatal: {error:#}"));
        std::process::exit(1);
    }
}

fn run_console() -> anyhow::Result<()> {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    let ctl = Arc::new(Ctl::new()?);
    CONSOLE_CTL.set(Arc::clone(&ctl)).ok();
    unsafe {
        SetConsoleCtrlHandler(Some(console_handler), true)?;
    }
    log("console mode");
    let user = service_user().unwrap_or_else(|_| install::current_user_sid().unwrap_or_default());
    pipe::serve(&ctl, &user);
    Ok(())
}

/// SetConsoleCtrlHandler callbacks cannot capture, so the control block is
/// parked in a global for the process lifetime (console mode only).
static CONSOLE_CTL: std::sync::OnceLock<Arc<Ctl>> = std::sync::OnceLock::new();

unsafe extern "system" fn console_handler(event: u32) -> windows::core::BOOL {
    use windows::Win32::System::Console::{CTRL_CLOSE_EVENT, CTRL_C_EVENT};
    if event == CTRL_C_EVENT || event == CTRL_CLOSE_EVENT {
        if let Some(ctl) = CONSOLE_CTL.get() {
            ctl.request_stop(None);
        }
        return true.into();
    }
    false.into()
}
