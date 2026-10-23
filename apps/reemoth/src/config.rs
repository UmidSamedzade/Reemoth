//! Persistent settings: signaling URL and, on hosts, identity and password.

use std::path::{Path, PathBuf};

use anyhow::Context;
use reemoth_auth::{HostIdentity, PasswordSecret};
use reemoth_proto::signal::HostId;
use serde::{Deserialize, Serialize};

/// Used until a deployed server is configured with `--signal`.
pub const DEFAULT_SIGNAL_URL: &str = "ws://127.0.0.1:8787";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HostConfig {
    pub id: u32,
    /// Ed25519 secret key, base64.
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<StoredPassword>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StoredPassword {
    pub salt: String,
    pub secret: String,
}

pub struct ConfigFile {
    path: PathBuf,
    pub config: Config,
}

impl ConfigFile {
    pub fn default_dir() -> anyhow::Result<PathBuf> {
        directories::ProjectDirs::from("", "", "Reemoth")
            .map(|d| d.config_dir().to_path_buf())
            .context("cannot determine the config directory")
    }

    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("config.json");
        let config = match std::fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, config })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let dir = self.path.parent().expect("config path has a parent");
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let tmp = self.path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        write_private(&tmp, serde_json::to_string_pretty(&self.config)?.as_bytes())?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    /// `--signal` > `REEMOTH_SIGNAL_URL` > saved > default. A URL given on the
    /// command line is saved for next time.
    pub fn resolve_signal_url(&mut self, flag: Option<String>) -> anyhow::Result<String> {
        if let Some(url) = flag {
            if self.config.signal_url.as_deref() != Some(&url) {
                self.config.signal_url = Some(url.clone());
                self.save()?;
            }
            return Ok(url);
        }
        if let Ok(url) = std::env::var("REEMOTH_SIGNAL_URL") {
            return Ok(url);
        }
        Ok(self
            .config
            .signal_url
            .clone()
            .unwrap_or_else(|| DEFAULT_SIGNAL_URL.to_owned()))
    }

    /// Loads the host identity, creating and saving one on first run.
    pub fn host_identity(&mut self) -> anyhow::Result<(HostId, HostIdentity)> {
        if let Some(h) = &self.config.host {
            let id = HostId::new(h.id).context("invalid host ID in config")?;
            return Ok((id, HostIdentity::from_base64(&h.key)?));
        }
        let identity = HostIdentity::generate();
        let id = random_host_id();
        self.config.host = Some(HostConfig {
            id: id.get(),
            key: identity.secret_base64(),
            password: None,
        });
        self.save()?;
        Ok((id, identity))
    }

    /// Discards the ID (keeps the password) so the next run picks a new one.
    pub fn reset_host_id(&mut self) -> anyhow::Result<()> {
        if let Some(h) = &mut self.config.host {
            h.id = random_host_id().get();
            h.key = HostIdentity::generate().secret_base64();
        }
        self.save()
    }

    pub fn host_password(&self) -> anyhow::Result<Option<(Vec<u8>, PasswordSecret)>> {
        let Some(p) = self.config.host.as_ref().and_then(|h| h.password.as_ref()) else {
            return Ok(None);
        };
        let salt = reemoth_auth::unb64(&p.salt, "salt")?;
        Ok(Some((salt, PasswordSecret::from_base64(&p.secret)?)))
    }

    pub fn set_host_password(&mut self, password: &str) -> anyhow::Result<()> {
        PasswordSecret::check_strength(password).map_err(anyhow::Error::msg)?;
        self.host_identity()?;
        let salt = PasswordSecret::new_salt();
        let secret = PasswordSecret::derive(password, &salt)?;
        self.config
            .host
            .as_mut()
            .expect("identity was just ensured")
            .password = Some(StoredPassword {
            salt: reemoth_auth::b64(&salt),
            secret: secret.to_base64(),
        });
        self.save()
    }
}

/// Nine digits without a leading zero, so it reads the same everywhere.
fn random_host_id() -> HostId {
    let raw = u32::from_le_bytes(reemoth_auth::random_bytes());
    HostId::new(100_000_000 + raw % 900_000_000).expect("in range")
}

/// Creates the file readable by the owner only (it holds the host's secrets).
fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // On Windows the per-user AppData directory is already private.
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}
