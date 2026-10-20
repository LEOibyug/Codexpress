use anyhow::Result;
use serde_json::Value;
use std::path::Path;

pub trait CredentialStore: Send + Sync {
    fn save(&self, source_id: &str, secret: &Value) -> Result<()>;
}

pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    pub fn new(state_dir: &Path) -> Self {
        use base64::Engine;
        let service = format!(
            "com.codexpress.upstream.{}",
            base64::engine::general_purpose::STANDARD
                .encode(state_dir.to_string_lossy().as_bytes())
        );
        Self { service }
    }
}

impl CredentialStore for KeychainStore {
    fn save(&self, source_id: &str, secret: &Value) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let bytes = serde_json::to_vec(secret)?;
            security_framework::passwords::set_generic_password(&self.service, source_id, &bytes)
                .map_err(|_| {
                anyhow::anyhow!(
                    "无法把更新后的官方凭据保存到 Codexpress 钥匙串，请检查钥匙串访问权限"
                )
            })?;
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (&self.service, source_id, secret);
            anyhow::bail!("此版本仅支持 macOS 钥匙串");
        }
    }
}
