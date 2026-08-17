//! Windows service integration: SCM dispatcher, control handler, and status
//! reporting. The actual work happens in `crate::run_server`, shared with
//! console mode.

use std::ffi::OsString;
use std::sync::Arc;
use std::time::Duration;

use log::{error, info};
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
    ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

/// SCM service name. Must match the `sc.exe create` name in
/// installer/spora.nsi.
const SERVICE_NAME: &str = "Spora";

define_windows_service!(ffi_service_main, service_main);

/// Connects the process to the SCM dispatcher. Blocks until the service
/// stops.
pub fn run() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        error!("service failed: {e}");
    }
}

fn run_service() -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = Arc::new(tokio::sync::Notify::new());

    let handler_shutdown = shutdown.clone();
    let status_handle =
        service_control_handler::register(SERVICE_NAME, move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                info!("SCM requested stop");
                handler_shutdown.notify_one();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        })?;

    let set_state = |state: ServiceState, exit_code: ServiceExitCode| {
        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code,
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })
    };

    // Startup is quick (create the pipe, bind the gRPC server), so go
    // straight to Running; a bind failure is reported through the Stopped
    // exit code below.
    set_state(ServiceState::Running, ServiceExitCode::Win32(0))?;

    let result = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt.block_on(crate::run_server(shutdown)),
        Err(e) => Err(e.into()),
    };

    match &result {
        Ok(()) => info!("service stopped cleanly"),
        Err(e) => error!("server error: {e}"),
    }

    set_state(
        ServiceState::Stopped,
        match result {
            Ok(()) => ServiceExitCode::Win32(0),
            Err(_) => ServiceExitCode::ServiceSpecific(1),
        },
    )?;

    Ok(())
}
