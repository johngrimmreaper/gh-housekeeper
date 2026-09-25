use crate::{
    DaemonControlHandle, DaemonEventSubscriptionError, ForegroundDaemonOptions,
    run_foreground_daemon_with_control,
};
use anyhow::{Context, Result, anyhow, bail};
use gh_housekeeper_core::{
    DAEMON_PROTOCOL_SCHEMA_VERSION, DaemonCommand, DaemonControlError, DaemonEvent,
    DaemonEventSubscriptionRequest, DaemonRequest, DaemonResponse, DaemonRuntimeState,
};
use gh_housekeeper_github::GithubClient;
use gh_housekeeper_storage::StatePaths;
use serde_json::Value;
use std::{
    env, fs, io,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::UnixStream as StdUnixStream,
    },
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

const APP_DIR: &str = "gh-housekeeper";
const SOCKET_FILE: &str = "control-v2.sock";
const MAX_FRAME_BYTES: usize = 64 * 1024;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn local_daemon_socket_path() -> Result<PathBuf> {
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && runtime_dir.is_absolute()
    {
        return Ok(runtime_dir.join(APP_DIR).join(SOCKET_FILE));
    }

    let paths = StatePaths::discover().context("failed to determine local gh-housekeeper paths")?;
    Ok(paths.state_dir.join("daemon").join(SOCKET_FILE))
}

pub async fn run_foreground_daemon_with_local_ipc(
    provider: Arc<GithubClient>,
    options: ForegroundDaemonOptions,
) -> Result<()> {
    let socket_path = local_daemon_socket_path()?;
    let (control_sender, control_receiver) = tokio::sync::oneshot::channel();

    let runtime = run_foreground_daemon_with_control(provider, options, move |control| {
        let _ = control_sender.send(control);
    });
    tokio::pin!(runtime);

    let control = tokio::select! {
        result = &mut runtime => return result,
        control = control_receiver => control
            .context("daemon exited before exposing its local control handle")?,
    };

    let server = run_local_daemon_server(socket_path, control.clone());
    tokio::pin!(server);

    tokio::select! {
        runtime_result = &mut runtime => {
            let server_result = server.await;
            runtime_result?;
            server_result?;
            Ok(())
        }
        server_result = &mut server => {
            match server_result {
                Ok(()) => runtime.await,
                Err(error) => {
                    control.shutdown();
                    let runtime_result = runtime.await;
                    if let Err(runtime_error) = runtime_result {
                        return Err(runtime_error.context(
                            "daemon runtime also failed after the local IPC server stopped"
                        ));
                    }
                    Err(error.context("local daemon IPC server failed"))
                }
            }
        }
    }
}

pub async fn send_local_daemon_request(command: DaemonCommand) -> Result<DaemonResponse> {
    let socket_path = local_daemon_socket_path()?;
    let request = DaemonRequest::new(next_request_id(), command);
    exchange_request(&socket_path, &request).await
}

async fn run_local_daemon_server(
    socket_path: PathBuf,
    control: DaemonControlHandle,
) -> Result<()> {
    if control.status().state == DaemonRuntimeState::Stopping {
        return Ok(());
    }

    let subscription_request = DaemonEventSubscriptionRequest::new("local-ipc-server");
    let mut events = control
        .subscribe(&subscription_request)
        .map_err(|error| anyhow!("failed to subscribe local IPC server to daemon events: {error:?}"))?;

    prepare_socket_path(&socket_path)?;
    let listener = UnixListener::bind(&socket_path).with_context(|| {
        format!(
            "failed to bind local daemon control socket {}",
            socket_path.display()
        )
    })?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "failed to restrict local daemon control socket permissions {}",
            socket_path.display()
        )
    })?;
    let _cleanup = SocketCleanup::new(socket_path.clone());

    loop {
        if control.status().state == DaemonRuntimeState::Stopping {
            return Ok(());
        }

        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.with_context(|| {
                    format!("failed to accept local daemon control connection on {}", socket_path.display())
                })?;
                if let Err(error) = handle_connection(&mut stream, &control).await {
                    eprintln!("local daemon control connection failed: {error:#}");
                }
            }
            event = events.recv() => {
                match event {
                    Ok(DaemonEvent::StatusChanged { status })
                        if status.state == DaemonRuntimeState::Stopping =>
                    {
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(DaemonEventSubscriptionError::Lagged(_)) => {}
                    Err(DaemonEventSubscriptionError::Closed) => return Ok(()),
                }
            }
        }
    }
}

async fn handle_connection(stream: &mut UnixStream, control: &DaemonControlHandle) -> Result<()> {
    let frame = match read_frame(stream).await {
        Ok(frame) => frame,
        Err(message) => {
            let response = DaemonResponse::error_for_request_id(
                "",
                DaemonControlError::invalid_request(message),
            );
            write_response(stream, &response).await?;
            return Ok(());
        }
    };

    let request_id = request_id_from_json(&frame);
    let request = match serde_json::from_slice::<DaemonRequest>(&frame) {
        Ok(request) => request,
        Err(error) => {
            let response = DaemonResponse::error_for_request_id(
                request_id,
                DaemonControlError::invalid_request(format!(
                    "invalid daemon control request JSON: {error}"
                )),
            );
            write_response(stream, &response).await?;
            return Ok(());
        }
    };

    let response = control.handle_request(&request);
    write_response(stream, &response).await
}

async fn exchange_request(socket_path: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    validate_client_socket_path(socket_path)?;
    let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
        format!(
            "failed to connect to local daemon control socket {}",
            socket_path.display()
        )
    })?;

    let mut encoded =
        serde_json::to_vec(request).context("failed to serialize daemon control request")?;
    if encoded.len() > MAX_FRAME_BYTES {
        bail!("daemon control request exceeds the local frame limit");
    }
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .await
        .context("failed to write daemon control request")?;
    stream
        .flush()
        .await
        .context("failed to flush daemon control request")?;

    let frame = read_frame(&mut stream)
        .await
        .map_err(|message| anyhow!("invalid daemon control response frame: {message}"))?;
    let response: DaemonResponse =
        serde_json::from_slice(&frame).context("failed to decode daemon control response")?;

    if response.schema_version != DAEMON_PROTOCOL_SCHEMA_VERSION {
        bail!(
            "daemon response schema mismatch: received {}, expected {}",
            response.schema_version,
            DAEMON_PROTOCOL_SCHEMA_VERSION
        );
    }
    if response.request_id != request.request_id {
        bail!(
            "daemon response request id mismatch: received {:?}, expected {:?}",
            response.request_id,
            request.request_id
        );
    }

    Ok(response)
}

async fn write_response(stream: &mut UnixStream, response: &DaemonResponse) -> Result<()> {
    let mut encoded =
        serde_json::to_vec(response).context("failed to serialize daemon control response")?;
    if encoded.len() > MAX_FRAME_BYTES {
        bail!("daemon control response exceeds the local frame limit");
    }
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .await
        .context("failed to write daemon control response")?;
    stream
        .flush()
        .await
        .context("failed to flush daemon control response")?;
    Ok(())
}

async fn read_frame(stream: &mut UnixStream) -> std::result::Result<Vec<u8>, String> {
    let mut frame = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];

    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("failed to read local control frame: {error}"))?;
        if read == 0 {
            return Err(if frame.is_empty() {
                "connection closed before a request frame was received".to_owned()
            } else {
                "connection closed before the request frame terminator".to_owned()
            });
        }

        if let Some(newline) = chunk[..read].iter().position(|byte| *byte == b'\n') {
            if frame.len() + newline > MAX_FRAME_BYTES {
                return Err(format!(
                    "local control frame exceeds the {MAX_FRAME_BYTES}-byte limit"
                ));
            }
            frame.extend_from_slice(&chunk[..newline]);

            if chunk[newline + 1..read]
                .iter()
                .any(|byte| !byte.is_ascii_whitespace())
            {
                return Err("multiple local control frames on one connection are not supported".to_owned());
            }
            if frame.is_empty() {
                return Err("empty local control frame".to_owned());
            }
            return Ok(frame);
        }

        if frame.len() + read > MAX_FRAME_BYTES {
            return Err(format!(
                "local control frame exceeds the {MAX_FRAME_BYTES}-byte limit"
            ));
        }
        frame.extend_from_slice(&chunk[..read]);
    }
}

fn request_id_from_json(frame: &[u8]) -> String {
    serde_json::from_slice::<Value>(frame)
        .ok()
        .and_then(|value| {
            value
                .get("request_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn validate_client_socket_path(socket_path: &Path) -> Result<()> {
    let metadata = match socket_path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!(
                "gh-housekeeper daemon control socket is not available at {}",
                socket_path.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect local daemon control socket {}",
                    socket_path.display()
                )
            });
        }
    };

    if metadata.file_type().is_symlink() {
        bail!(
            "refusing symbolic link daemon control socket {}",
            socket_path.display()
        );
    }
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing non-socket daemon control path {}",
            socket_path.display()
        );
    }
    Ok(())
}

fn prepare_socket_path(socket_path: &Path) -> Result<()> {
    let directory = socket_path
        .parent()
        .context("local daemon control socket has no parent directory")?;
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "failed to create local daemon control directory {}",
            directory.display()
        )
    })?;

    let directory_metadata = directory.symlink_metadata().with_context(|| {
        format!(
            "failed to inspect local daemon control directory {}",
            directory.display()
        )
    })?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        bail!(
            "refusing unsafe local daemon control directory {}",
            directory.display()
        );
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "failed to restrict local daemon control directory permissions {}",
            directory.display()
        )
    })?;

    match socket_path.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "refusing symbolic link daemon control socket {}",
                socket_path.display()
            );
        }
        Ok(metadata) if metadata.file_type().is_socket() => {
            match StdUnixStream::connect(socket_path) {
                Ok(_) => {
                    bail!(
                        "daemon control socket is already accepting connections at {}; refusing to replace it",
                        socket_path.display()
                    );
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    fs::remove_file(socket_path).with_context(|| {
                        format!(
                            "failed to remove stale daemon control socket {}",
                            socket_path.display()
                        )
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to verify whether daemon control socket is stale {}",
                            socket_path.display()
                        )
                    });
                }
            }
        }
        Ok(_) => {
            bail!(
                "refusing to replace non-socket daemon control path {}",
                socket_path.display()
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect local daemon control socket {}",
                    socket_path.display()
                )
            });
        }
    }

    Ok(())
}

fn next_request_id() -> String {
    format!(
        "cli-{}-{}",
        process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

struct SocketCleanup {
    path: PathBuf,
}

impl SocketCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        if self
            .path
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use gh_housekeeper_core::{
        DaemonControlErrorCode, DaemonReply, DaemonResponseOutcome, DaemonStatus,
        monitoring_scheduler_cancellation,
    };
    use std::{
        os::unix::{fs::symlink, net::UnixListener as StdUnixListener},
        sync::{Arc, Mutex},
        time::Duration,
    };

    fn test_socket_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "gh-housekeeper-local-ipc-{name}-{}-{}",
                process::id(),
                REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ))
            .join(SOCKET_FILE)
    }

    fn control_fixture() -> (
        DaemonControlHandle,
        gh_housekeeper_core::MonitoringSchedulerShutdown,
    ) {
        let (cancellation, shutdown) = monitoring_scheduler_cancellation();
        let mut status = DaemonStatus::starting(Utc::now());
        status.state = DaemonRuntimeState::Running;
        (
            DaemonControlHandle::new(Arc::new(Mutex::new(status)), cancellation),
            shutdown,
        )
    }

    async fn wait_for_socket(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("socket did not appear at {}", path.display());
    }

    #[tokio::test]
    async fn local_socket_serves_live_status_and_graceful_shutdown() {
        let path = test_socket_path("status-shutdown");
        let parent = path.parent().unwrap().to_path_buf();
        let (control, shutdown) = control_fixture();
        let server_control = control.clone();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run_local_daemon_server(server_path, server_control).await
        });
        wait_for_socket(&path).await;
        let socket_mode = path.symlink_metadata().unwrap().permissions().mode() & 0o777;
        let directory_mode = parent.symlink_metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(socket_mode, 0o600);
        assert_eq!(directory_mode, 0o700);

        let status_request = DaemonRequest::new("status-test", DaemonCommand::Status);
        let status_response = exchange_request(&path, &status_request).await.unwrap();
        assert!(matches!(
            status_response.outcome,
            DaemonResponseOutcome::Ok {
                reply: DaemonReply::Status { status }
            } if status.state == DaemonRuntimeState::Running
        ));

        let shutdown_request = DaemonRequest::new("shutdown-test", DaemonCommand::Shutdown);
        let shutdown_response = exchange_request(&path, &shutdown_request).await.unwrap();
        assert!(matches!(
            shutdown_response.outcome,
            DaemonResponseOutcome::Ok {
                reply: DaemonReply::ShutdownAccepted
            }
        ));
        assert!(shutdown.is_cancelled());
        server.await.unwrap().unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn stale_socket_is_recovered_after_runtime_lock_authority() {
        let path = test_socket_path("stale");
        let parent = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent).unwrap();
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        assert!(path.exists());

        let (control, _) = control_fixture();
        let server_control = control.clone();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run_local_daemon_server(server_path, server_control).await
        });
        wait_for_socket(&path).await;

        let response = exchange_request(
            &path,
            &DaemonRequest::new("stale-status", DaemonCommand::Status),
        )
        .await
        .unwrap();
        assert!(matches!(
            response.outcome,
            DaemonResponseOutcome::Ok {
                reply: DaemonReply::Status { .. }
            }
        ));

        control.shutdown();
        server.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn malformed_and_oversized_frames_fail_structurally() {
        let path = test_socket_path("bad-frames");
        let parent = path.parent().unwrap().to_path_buf();
        let (control, _) = control_fixture();
        let server_control = control.clone();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run_local_daemon_server(server_path, server_control).await
        });
        wait_for_socket(&path).await;

        let mut malformed = UnixStream::connect(&path).await.unwrap();
        malformed.write_all(b"{not-json}\n").await.unwrap();
        let frame = read_frame(&mut malformed).await.unwrap();
        let response: DaemonResponse = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(
            response.outcome,
            DaemonResponseOutcome::Error { error }
                if error.code == DaemonControlErrorCode::InvalidRequest
        ));

        let mut incomplete = UnixStream::connect(&path).await.unwrap();
        incomplete
            .write_all(b"{\"schema_version\":2")
            .await
            .unwrap();
        incomplete.shutdown().await.unwrap();
        let frame = read_frame(&mut incomplete).await.unwrap();
        let response: DaemonResponse = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(
            response.outcome,
            DaemonResponseOutcome::Error { error }
                if error.code == DaemonControlErrorCode::InvalidRequest
        ));

        let mut oversized = UnixStream::connect(&path).await.unwrap();
        oversized
            .write_all(&vec![b'x'; MAX_FRAME_BYTES + 1])
            .await
            .unwrap();
        oversized.write_all(b"\n").await.unwrap();
        let frame = read_frame(&mut oversized).await.unwrap();
        let response: DaemonResponse = serde_json::from_slice(&frame).unwrap();
        assert!(matches!(
            response.outcome,
            DaemonResponseOutcome::Error { error }
                if error.code == DaemonControlErrorCode::InvalidRequest
        ));

        control.shutdown();
        server.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn run_monitoring_now_remains_explicitly_unsupported() {
        let path = test_socket_path("run-now");
        let parent = path.parent().unwrap().to_path_buf();
        let (control, shutdown) = control_fixture();
        let server_control = control.clone();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run_local_daemon_server(server_path, server_control).await
        });
        wait_for_socket(&path).await;

        let response = exchange_request(
            &path,
            &DaemonRequest::new("run-now-test", DaemonCommand::RunMonitoringNow),
        )
        .await
        .unwrap();
        assert!(matches!(
            response.outcome,
            DaemonResponseOutcome::Error { error }
                if error.code == DaemonControlErrorCode::Unsupported
        ));
        assert!(!shutdown.is_cancelled());
        assert_eq!(control.status().state, DaemonRuntimeState::Running);

        control.shutdown();
        server.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn schema_mismatch_is_returned_without_executing_shutdown() {
        let path = test_socket_path("schema");
        let parent = path.parent().unwrap().to_path_buf();
        let (control, shutdown) = control_fixture();
        let server_control = control.clone();
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            run_local_daemon_server(server_path, server_control).await
        });
        wait_for_socket(&path).await;

        let mut request = DaemonRequest::new("schema-test", DaemonCommand::Shutdown);
        request.schema_version += 1;
        let response = exchange_request(&path, &request).await.unwrap();
        assert!(matches!(
            response.outcome,
            DaemonResponseOutcome::Error { error }
                if error.code == DaemonControlErrorCode::SchemaMismatch
        ));
        assert!(!shutdown.is_cancelled());
        assert_eq!(control.status().state, DaemonRuntimeState::Running);

        control.shutdown();
        server.await.unwrap().unwrap();
        let _ = fs::remove_dir_all(parent);
    }

    #[tokio::test]
    async fn live_socket_is_never_replaced_as_stale() {
        let path = test_socket_path("live");
        let parent = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent).unwrap();
        let listener = StdUnixListener::bind(&path).unwrap();

        let error = prepare_socket_path(&path).unwrap_err();
        assert!(error.to_string().contains("already accepting connections"));
        assert!(path.exists());

        drop(listener);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn unsafe_socket_symlink_is_refused() {
        let path = test_socket_path("symlink");
        let parent = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&parent).unwrap();
        let target = parent.join("target");
        fs::write(&target, b"not a socket").unwrap();
        symlink(&target, &path).unwrap();

        let error = prepare_socket_path(&path).unwrap_err();
        assert!(error.to_string().contains("symbolic link"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(target);
        let _ = fs::remove_dir_all(parent);
    }
}
