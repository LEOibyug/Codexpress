use anyhow::{bail, Context, Result};
use codexpress_gateway::{config, management, model::AppState, relay};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

#[tokio::main]
async fn main() {
    if let Err(error)=run().await {eprintln!("Codexpress: {error}");std::process::exit(1);}
}

async fn run()->Result<()> {
    let mut args=std::env::args().skip(1);let mut state_dir=None;let mut port=17863;
    while let Some(arg)=args.next(){match arg.as_str(){
        "--state-dir"=>state_dir=Some(PathBuf::from(args.next().context("缺少 state-dir")?)),
        "--port"=>port=args.next().context("缺少端口")?.parse::<u16>().context("端口格式错误")?,
        "--version"=>{println!("codexpress-gateway 0.1.0");return Ok(());},
        _=>bail!("未知启动参数"),
    }}
    let state_dir=state_dir.context("必须指定 --state-dir")?;
    if !state_dir.is_absolute() || port==0 {bail!("状态目录必须为绝对路径，端口必须大于零");}
    fs::create_dir_all(&state_dir)?;fs::set_permissions(&state_dir,fs::Permissions::from_mode(0o700))?;
    let key_path=state_dir.join("gateway-key");
    let key=if key_path.exists(){fs::read_to_string(&key_path)?}else{
        let key=format!("cx-{}{}",uuid::Uuid::new_v4().simple(),uuid::Uuid::new_v4().simple());
        config::secure_write(&key_path,key.as_bytes())?;key
    };
    if key.trim().len()<32 {bail!("本地访问凭据无效，请检查应用状态目录");}
    let state=AppState::new(state_dir.clone(),port,key.trim().into())?;
    let listener=tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,port)).await.context("本地端口被占用，未更改 Codex 连接")?;
    let management=management::serve(state.clone());
    let server=axum::serve(listener,relay::router(state));
    tokio::select!{
        result=management=>result?,
        result=server=>result?,
        _=tokio::signal::ctrl_c()=>{},
    }
    let _=fs::remove_file(state_dir.join("control.sock"));
    Ok(())
}
