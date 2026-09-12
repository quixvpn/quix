//! Windows service integration.
//!
//! The Service Control Manager starts a service and then waits for it to report
//! back over the service-control protocol. A plain console binary never does,
//! so the SCM marks it failed and terminates it — `sc create` on an unmodified
//! executable does not produce a working service. This module implements that
//! protocol, and reports `Stopped` when the daemon exits.

use std::ffi::OsString;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
	ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
	ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

pub const SERVICE_NAME: &str = "quixd";

const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

define_windows_service!(ffi_service_main, service_main);

/// Hands control to the SCM, returning false when we were not started by it —
/// which is how running from a terminal is detected, since the dispatcher only
/// connects successfully inside a real service process.
pub fn run() -> bool {
	service_dispatcher::start(SERVICE_NAME, ffi_service_main).is_ok()
}

fn service_main(_args: Vec<OsString>) {
	if let Err(e) = serve() {
		crate::warn!("service failed: {e}");
	}
}

fn serve() -> anyhow::Result<()> {
	let (shutdown_tx, shutdown_rx) = mpsc::channel();

	let handler = move |control| match control {
		// Interrogate must be answered or the SCM considers us unresponsive.
		ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
		ServiceControl::Stop | ServiceControl::Shutdown => {
			let _ = shutdown_tx.send(());
			ServiceControlHandlerResult::NoError
		}
		_ => ServiceControlHandlerResult::NotImplemented,
	};

	let status_handle = service_control_handler::register(SERVICE_NAME, handler)?;

	let report = |state, controls, exit_code| ServiceStatus {
		service_type: SERVICE_TYPE,
		current_state: state,
		controls_accepted: controls,
		exit_code,
		checkpoint: 0,
		wait_hint: Duration::default(),
		process_id: None,
	};

	status_handle.set_service_status(report(
		ServiceState::Running,
		ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
		ServiceExitCode::Win32(0),
	))?;

	// The daemon owns the async runtime; this thread only waits for the SCM.
	let result = crate::runtime()?.block_on(crate::run(async move {
		let _ = tokio::task::spawn_blocking(move || shutdown_rx.recv()).await;
	}));

	let exit_code = match &result {
		Ok(()) => ServiceExitCode::Win32(0),
		Err(e) => {
			crate::warn!("daemon exited with an error: {e:#}");
			ServiceExitCode::ServiceSpecific(1)
		}
	};

	status_handle.set_service_status(report(
		ServiceState::Stopped,
		ServiceControlAccept::empty(),
		exit_code,
	))?;

	result
}
