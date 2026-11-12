//! Saved sessions and known host keys, persisted as JSON in the user's
//! configuration directory.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    Password,
    PublicKey,
}

impl AuthMethod {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Password => "Password",
            Self::PublicKey => "Public key",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TunnelKind {
    #[default]
    Local,
    Remote,
    Dynamic,
}

impl TunnelKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::Remote => "Remote",
            Self::Dynamic => "SOCKS5",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            Self::Local => "Listen on this machine, connect from the SSH server.",
            Self::Remote => "The SSH server listens, connections come back to this machine.",
            Self::Dynamic => "Local SOCKS5 proxy; destinations are chosen by the client.",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Tunnel {
    pub kind: TunnelKind,
    pub bind_host: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

impl Default for Tunnel {
    fn default() -> Self {
        Self {
            kind: TunnelKind::Local,
            bind_host: "127.0.0.1".to_owned(),
            bind_port: 8080,
            target_host: String::new(),
            target_port: 80,
        }
    }
}

impl Tunnel {
    pub fn summary(&self) -> String {
        match self.kind {
            TunnelKind::Local => format!(
                "L  {}:{}  ->  {}:{}",
                self.bind_host, self.bind_port, self.target_host, self.target_port
            ),
            TunnelKind::Remote => format!(
                "R  {}:{}  ->  {}:{}",
                self.bind_host, self.bind_port, self.target_host, self.target_port
            ),
            TunnelKind::Dynamic => {
                format!("D  {}:{}  (SOCKS5)", self.bind_host, self.bind_port)
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.bind_host.trim().is_empty() {
            return Err("bind address is required".into());
        }
        if self.bind_port == 0 {
            return Err("bind port must be between 1 and 65535".into());
        }
        if self.kind != TunnelKind::Dynamic {
            if self.target_host.trim().is_empty() {
                return Err("target host is required".into());
            }
            if self.target_port == 0 {
                return Err("target port must be between 1 and 65535".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionProfile {
    pub id: u64,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub key_path: String,
    pub terminal_type: String,
    pub keepalive_secs: u16,
    pub tunnels: Vec<Tunnel>,
}

impl Default for SessionProfile {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            host: String::new(),
            port: 22,
            username: default_username(),
            auth: AuthMethod::Password,
            key_path: String::new(),
            terminal_type: "xterm-256color".to_owned(),
            keepalive_secs: 30,
            tunnels: Vec::new(),
        }
    }
}

impl SessionProfile {
    pub fn display_name(&self) -> String {
        if self.name.trim().is_empty() {
            format!("{}@{}", self.username, self.host)
        } else {
            self.name.clone()
        }
    }

    pub fn target(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() {
            return Err("Host is required.".into());
        }
        if self.port == 0 {
            return Err("Port must be between 1 and 65535.".into());
        }
        if self.username.trim().is_empty() {
            return Err("Username is required.".into());
        }
        if self.auth == AuthMethod::PublicKey && self.key_path.trim().is_empty() {
            return Err("Choose a private key file, or switch to password auth.".into());
        }
        for (index, tunnel) in self.tunnels.iter().enumerate() {
            tunnel
                .validate()
                .map_err(|error| format!("Tunnel {}: {error}.", index + 1))?;
        }
        Ok(())
    }
}

fn default_username() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionStore {
    #[serde(default)]
    pub sessions: Vec<SessionProfile>,
    #[serde(default)]
    next_id: u64,
}

impl SessionStore {
    pub fn load(path: &Path) -> Self {
        let mut store: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        // Repair files written before next_id was tracked (or hand-edited files).
        let highest = store.sessions.iter().map(|s| s.id).max().unwrap_or(0);
        store.next_id = store.next_id.max(highest.saturating_add(1)).max(1);
        store
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        save_json(path, self)
    }

    pub fn upsert(&mut self, mut profile: SessionProfile) -> u64 {
        if profile.id == 0 {
            self.next_id = self.next_id.max(1);
            profile.id = self.next_id;
            self.next_id += 1;
        } else {
            self.next_id = self.next_id.max(profile.id + 1);
        }

        if let Some(existing) = self.sessions.iter_mut().find(|s| s.id == profile.id) {
            *existing = profile.clone();
        } else {
            self.sessions.push(profile.clone());
        }
        self.sessions
            .sort_by_key(|profile| profile.display_name().to_lowercase());
        profile.id
    }

    pub fn remove(&mut self, id: u64) {
        self.sessions.retain(|s| s.id != id);
    }

    pub fn get(&self, id: u64) -> Option<&SessionProfile> {
        self.sessions.iter().find(|s| s.id == id)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KnownHost {
    pub algorithm: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KnownHosts {
    #[serde(default)]
    pub hosts: BTreeMap<String, KnownHost>,
}

impl KnownHosts {
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        save_json(path, self)
    }

    pub fn get(&self, host_key: &str) -> Option<&KnownHost> {
        self.hosts.get(host_key)
    }

    pub fn insert(&mut self, host_key: String, algorithm: String, fingerprint: String) {
        self.hosts.insert(
            host_key,
            KnownHost {
                algorithm,
                fingerprint,
            },
        );
    }
}

/// Key used in known_hosts: `host` for the default port, `host:port` otherwise.
pub fn host_key(host: &str, port: u16) -> String {
    let host = host.trim().to_lowercase();
    if port == 22 {
        host
    } else if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_error) if path.exists() => {
            // Windows cannot rename over an existing file.
            std::fs::remove_file(path)?;
            std::fs::rename(&tmp, path)
        }
        Err(error) => Err(error),
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        PathBuf::from(appdata).join("Rustty")
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("rustty")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config").join("rustty")
    } else {
        PathBuf::from(".rustty")
    }
}

pub fn sessions_path() -> PathBuf {
    config_dir().join("sessions.json")
}

pub fn known_hosts_path() -> PathBuf {
    config_dir().join("known_hosts.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_omits_default_port() {
        assert_eq!(host_key("Example.COM", 22), "example.com");
        assert_eq!(host_key("example.com", 2222), "example.com:2222");
        assert_eq!(host_key("::1", 2222), "[::1]:2222");
    }

    #[test]
    fn upsert_assigns_stable_ids() {
        let mut store = SessionStore::default();
        let id = store.upsert(SessionProfile::default());
        assert_ne!(id, 0);
        let mut profile = store.get(id).unwrap().clone();
        profile.name = "Renamed".into();
        assert_eq!(store.upsert(profile), id);
        assert_eq!(store.sessions.len(), 1);
        assert_eq!(store.sessions[0].name, "Renamed");
    }

    #[test]
    fn load_repairs_missing_next_id() {
        let path =
            std::env::temp_dir().join(format!("rustty-sessions-test-{}.json", std::process::id()));
        std::fs::write(
            &path,
            r#"{"sessions":[{"id":7,"name":"seven","host":"example.com"}]}"#,
        )
        .unwrap();
        let store = SessionStore::load(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(store.sessions.len(), 1);
        assert_eq!(store.next_id, 8);
    }

    #[test]
    fn tunnel_validation_and_summary() {
        let mut tunnel = Tunnel {
            target_host: "db.internal".into(),
            target_port: 5432,
            ..Default::default()
        };
        assert!(tunnel.validate().is_ok());
        assert!(tunnel.summary().contains("127.0.0.1:8080"));
        assert!(tunnel.summary().contains("db.internal:5432"));

        tunnel.kind = TunnelKind::Dynamic;
        tunnel.target_host.clear();
        tunnel.target_port = 0;
        assert!(tunnel.validate().is_ok());
        assert!(tunnel.summary().contains("SOCKS5"));

        let invalid = Tunnel::default();
        assert!(invalid.validate().is_err());
    }
}
