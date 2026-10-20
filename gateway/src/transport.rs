use crate::model::Source;
use anyhow::{bail, Result};

/// Every caller uses this policy, including OAuth and quota operations. In
/// particular, a failed explicit proxy never degrades to a direct connection.
pub fn client(source: &Source) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(15));
    match source.proxy.mode.as_str() {
        "direct" if source.kind != "official" => {}
        "explicit" => {
            let address = source
                .proxy
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("来源未配置代理"))?;
            let parsed = url::Url::parse(address).map_err(|_| anyhow::anyhow!("代理地址无效"))?;
            if !["http", "https", "socks5", "socks5h"].contains(&parsed.scheme()) {
                bail!("代理协议不受支持");
            }
            builder = builder
                .proxy(reqwest::Proxy::all(address).map_err(|_| anyhow::anyhow!("代理地址无效"))?);
        }
        _ => bail!("来源的网络策略无效"),
    }
    // Decompression features are intentionally disabled in Cargo.toml. Raw
    // Content-Encoding and the original response bytes remain paired.
    builder
        .build()
        .map_err(|_| anyhow::anyhow!("无法创建来源连接"))
}
