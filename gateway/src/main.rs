use anyhow::{bail, Context, Result};
use codexpress_gateway::{config, credentials::ManagedCredentialStore, management, model::AppState};
use std::{
    fs,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::PathBuf,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Codexpress: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut state_dir = None;
    let mut port = 17863;
    let mut hold_ingress = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => {
                state_dir = Some(PathBuf::from(args.next().context("缺少 state-dir")?))
            }
            "--port" => {
                port = args
                    .next()
                    .context("缺少端口")?
                    .parse::<u16>()
                    .context("端口格式错误")?
            }
            "--hold-ingress" => hold_ingress = true,
            "--version" => {
                println!("codexpress-gateway {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => bail!("未知启动参数"),
        }
    }
    let state_dir = state_dir.context("必须指定 --state-dir")?;
    if !state_dir.is_absolute() || port == 0 {
        bail!("状态目录必须为绝对路径，端口必须大于零");
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&state_dir)?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&state_dir)
        .context("状态目录须为普通目录，不能是符号链接")?;
    if directory.metadata()?.uid() != unsafe { libc::geteuid() } {
        bail!("状态目录须属于当前用户");
    }
    directory.set_permissions(fs::Permissions::from_mode(0o700))?;
    drop(directory);
    let process_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(state_dir.join("gateway.lock"))?;
    // Held for the full process lifetime. This also prevents simultaneous
    // startup on different ports from racing over the same control socket.
    if unsafe { libc::flock(process_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("此状态目录已有网关进程，未接管已有实例");
    }
    let key_path = state_dir.join("gateway-key");
    let key = if key_path.exists() {
        fs::read_to_string(&key_path)?
    } else {
        let key = format!(
            "cx-{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        config::secure_write(&key_path, key.as_bytes())?;
        key
    };
    if key.trim().len() < 32 {
        bail!("本地访问凭据无效，请检查应用状态目录");
    }
    let credential_store = std::sync::Arc::new(ManagedCredentialStore::open(&state_dir)?);
    let state = AppState::new_with_credential_store(
        state_dir.clone(),
        port,
        key.trim().into(),
        Some(credential_store),
    )?;
    state.hold_gateway_ingress(hold_ingress);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
        .context("本地端口被占用，未更改 Codex 连接")?;
    let management = management::serve(state.clone());
    if codexpress_gateway::chatbot::resume(&state).await.is_err() {
        eprintln!("聊天客户端入口未能启动，请在应用中检查其独立端口");
    }
    if codexpress_gateway::images::resume(&state).await.is_err() {
        eprintln!("图片生成入口未能启动，请在应用中检查其独立端口");
    }
    if codexpress_gateway::control::resume(&state).await.is_err() {
        eprintln!("管理 API 未能启动，请在应用中检查其独立端口");
    }
    codexpress_gateway::balance::billing::start(&state);
    let server = axum::serve(
        listener,
        codexpress_gateway::remote::router(state.clone())
            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
    );
    tokio::select! {
        result=management=>result?,
        result=server=>result?,
        _=tokio::signal::ctrl_c()=>{},
        _=state.wait_for_gateway_shutdown()=>{},
    }
    // The OS releases listeners on exit. Keep the socket path: the next owner
    // checks liveness before removing it, so a shutdown cannot unlink another
    // process's socket after a race or a failed startup.
    Ok(())
}
