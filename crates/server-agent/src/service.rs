//! Windows service entry. The service process still reads the admin key from
//! the environment or from agent.secret; nothing is compiled into the binary.

use std::ffi::OsString;
use std::time::Duration;

use windows_service::define_windows_service;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;

const NAME: &str = "uXaServerAgent";

define_windows_service!(ffi_service_main, service_main);

fn service_main(_arguments: Vec<OsString>) {
    if let Err(err) = run_service_inner() {
        tracing::error!("service stopped: {err}");
    }
}

fn status(state: ServiceState) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(10),
        process_id: None,
    }
}

fn run_service_inner() -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = service_control_handler::register(NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Interrogate | ServiceControl::Shutdown => {
            if matches!(control, ServiceControl::Stop | ServiceControl::Shutdown) {
                let _ = tx.send(());
            }
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .map_err(|err| err.to_string())?;
    handle
        .set_service_status(status(ServiceState::Running))
        .map_err(|err| err.to_string())?;

    let worker = std::thread::spawn(|| {
        let config = match crate::load_config() {
            Ok(config) => config,
            Err(err) => {
                record_start_error(&err);
                return Err(err);
            }
        };
        crate::init_logging(&config.log_dir);
        let agent = match crate::Agent::new(config) {
            Ok(agent) => agent,
            Err(err) => {
                record_start_error(&err);
                return Err(err);
            }
        };
        let runtime = tokio::runtime::Runtime::new().map_err(|err| err.to_string())?;
        runtime.block_on(crate::serve(agent))
    });

    let _ = rx.recv();
    handle
        .set_service_status(status(ServiceState::Stopped))
        .map_err(|err| err.to_string())?;
    // The dedicated server is left running. Stopping the agent only closes the API.
    let _ = worker;
    std::process::exit(0);
}

fn record_start_error(err: &str) {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::fs::write(dir.join("agent-start-error.txt"), err);
        }
    }
}

pub fn run_service() -> Result<(), String> {
    service_dispatcher::start(NAME, ffi_service_main).map_err(|err| err.to_string())
}
