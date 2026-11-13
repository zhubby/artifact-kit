use super::*;
use std::{ffi::OsString, path::PathBuf, sync::OnceLock, time::Duration};
use tokio_util::sync::CancellationToken;
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

static CONFIG: OnceLock<PathBuf> = OnceLock::new();
define_windows_service!(ffi_main, service_main);
pub fn dispatch(config: PathBuf) -> Result<()> {
    CONFIG
        .set(config)
        .map_err(|_| anyhow::anyhow!("service already initialized"))?;
    service_dispatcher::start(NAME, ffi_main)?;
    Ok(())
}
fn service_main(_: Vec<OsString>) {
    if run_service().is_err() {
        std::process::exit(1);
    }
}
fn run_service() -> Result<()> {
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let handle = service_control_handler::register(NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            stop.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let status = |state, code| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: 0,
        wait_hint: Duration::from_secs(15),
        process_id: None,
    };
    handle.set_service_status(status(ServiceState::StartPending, 0))?;
    let config = CONFIG.get().context("service configuration unavailable")?;
    let cfg = crate::config::Config::load(config)?;
    let runtime = tokio::runtime::Runtime::new()?;
    handle.set_service_status(status(ServiceState::Running, 0))?;
    let result = runtime.block_on(crate::run_configured(config, cfg, cancel));
    handle.set_service_status(status(
        ServiceState::Stopped,
        if result.is_ok() { 0 } else { 1 },
    ))?;
    result
}
pub(super) fn platform_manage(action: &str, config: &Path, user: Option<&str>) -> Result<()> {
    let rights = ServiceManagerAccess::CONNECT
        | if action == "install" {
            ServiceManagerAccess::CREATE_SERVICE
        } else {
            ServiceManagerAccess::empty()
        };
    let manager = ServiceManager::local_computer(None::<&str>, rights)?;
    if action == "install" {
        crate::config::Config::load(config)?;
        let info = ServiceInfo {
            name: NAME.into(),
            display_name: "Artifactory Emissary".into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: std::env::current_exe()?,
            launch_arguments: vec![
                "service-run".into(),
                "--config".into(),
                config.as_os_str().into(),
            ],
            dependencies: vec![],
            account_name: user.map(OsString::from),
            account_password: std::env::var_os("ARTIFACTORY_SERVICE_PASSWORD"),
        };
        manager.create_service(&info, ServiceAccess::QUERY_STATUS)?;
        return Ok(());
    }
    let service = manager.open_service(
        NAME,
        ServiceAccess::START
            | ServiceAccess::STOP
            | ServiceAccess::QUERY_STATUS
            | ServiceAccess::DELETE,
    )?;
    if matches!(action, "stop" | "restart" | "uninstall")
        && service.query_status()?.current_state != ServiceState::Stopped
    {
        service.stop()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while service.query_status()?.current_state != ServiceState::Stopped {
            ensure!(
                std::time::Instant::now() < deadline,
                "service stop timed out"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    match action {
        "uninstall" => service.delete()?,
        "start" | "restart" => service.start::<&str>(&[])?,
        _ => {}
    }
    Ok(())
}
