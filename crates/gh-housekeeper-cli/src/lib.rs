mod daemon_runtime;
#[cfg(unix)]
mod local_ipc;

pub use daemon_runtime::{
    DaemonControlHandle, DaemonEventSubscription, DaemonEventSubscriptionError, DaemonOutputFormat,
    DaemonPresentation, ForegroundDaemonOptions, run_foreground_daemon,
    run_foreground_daemon_with_control,
};

#[cfg(unix)]
pub use local_ipc::{
    local_daemon_socket_path, run_foreground_daemon_with_local_ipc, send_local_daemon_request,
};
