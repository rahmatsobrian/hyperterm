//! Known Hosts
//!
//! A `known_hosts`-equivalent persistent store for SSH host key
//! verification, replacing Phase 1's "accept any key" placeholder in
//! `ClientHandler::check_server_key`.
//!
//! Storage format (one entry per line, OpenSSH-compatible enough to read at
//! a glance): `<host>:<port> <key-algorithm> <base64-public-key>`
//!
//! ## Trust model
//! - **Known + matching** -> silently accepted (the normal case after the
//!   first connection to a host).
//! - **Unknown host** -> handled per [`HostKeyPolicy`]: `Strict` rejects,
//!   `TrustOnFirstUse` auto-accepts and persists (logging a warning),
//!   `Prompt` asks the user interactively and persists only on explicit
//!   "yes".
//! - **Known host, but the key on the wire doesn't match what's stored**
//!   -> **always rejected**, regardless of policy. This is the actual
//!   MITM protection; a changed host key is exactly the scenario
//!   known_hosts exists to catch, so no policy is allowed to bypass it.
//!   The user must explicitly remove the stale entry (Phase 2.1: a
//!   `hyperterm --forget-host <host>` CLI flag; today, edit the file).

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use russh::keys::key::PublicKey;
use russh::keys::PublicKeyBase64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HostKeyPolicy {
    /// Ask the user interactively on first connection to a host; refuse if
    /// they don't explicitly accept. Default -- matches how every other
    /// SSH client behaves out of the box.
    Prompt,
    /// Automatically trust and persist unknown host keys without asking.
    /// Useful for scripted/automated use where no human is watching a
    /// prompt; still refuses on a *changed* key.
    Tofu,
    /// Refuse any host key not already present in the known_hosts store.
    /// No prompting, no auto-trust -- for environments where the
    /// known_hosts file is provisioned out-of-band (e.g. by config
    /// management) and an unexpected new host should hard-fail.
    Strict,
}

pub enum Verdict {
    Matches,
    New,
    Mismatch {
        stored_algo: String,
        stored_fingerprint: String,
    },
}

pub struct KnownHostsStore {
    path: PathBuf,
    entries: HashMap<String, (String, String)>, // "host:port" -> (algo, base64_key)
}

impl KnownHostsStore {
    pub fn default_path() -> PathBuf {
        if let Some(proj) = directories::ProjectDirs::from("dev", "HyperTerm", "HyperTerm") {
            proj.config_dir().join("known_hosts")
        } else {
            PathBuf::from("known_hosts")
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut entries = HashMap::new();
        if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("reading known_hosts at {path:?}"))?;
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.splitn(3, ' ');
                if let (Some(host_port), Some(algo), Some(key_b64)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    entries.insert(
                        host_port.to_string(),
                        (algo.to_string(), key_b64.to_string()),
                    );
                }
            }
        }
        tracing::info!(
            target: "hyperterm::ssh_engine::known_hosts",
            "loaded {} known host key(s) from {:?}",
            entries.len(),
            path
        );
        Ok(Self {
            path: path.to_path_buf(),
            entries,
        })
    }

    pub fn check(&self, host_port: &str, key: &PublicKey) -> Verdict {
        let incoming_b64 = key.public_key_base64();
        match self.entries.get(host_port) {
            None => Verdict::New,
            Some((_stored_algo, stored_b64)) if *stored_b64 == incoming_b64 => Verdict::Matches,
            Some((stored_algo, stored_b64)) => Verdict::Mismatch {
                stored_algo: stored_algo.clone(),
                stored_fingerprint: fingerprint_of_base64(stored_b64),
            },
        }
    }

    /// Persist a newly-trusted host key. Appends to the file (durable,
    /// crash-safe in the same spirit as the disk cache: never rewrites
    /// existing lines).
    pub fn trust(&mut self, host_port: &str, key: &PublicKey) -> Result<()> {
        let algo = key.name().to_string();
        let b64 = key.public_key_base64();

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{host_port} {algo} {b64}")?;

        self.entries.insert(host_port.to_string(), (algo, b64));
        tracing::info!(
            target: "hyperterm::ssh_engine::known_hosts",
            "trusted new host key for {} (fingerprint {})",
            host_port,
            key.fingerprint()
        );
        Ok(())
    }

    /// Removes a host's entry, e.g. after a legitimate key change (server
    /// reinstalled, etc.). Rewrites the whole file (entries are few enough
    /// that this is simpler and safer than trying to patch an append-only
    /// file in place). Returns `true` if an entry was actually removed.
    pub fn forget(&mut self, host_port: &str) -> Result<bool> {
        let removed = self.entries.remove(host_port).is_some();
        if removed {
            self.rewrite_file()?;
            tracing::info!(
                target: "hyperterm::ssh_engine::known_hosts",
                "removed known_hosts entry for '{}'",
                host_port
            );
        }
        Ok(removed)
    }

    fn rewrite_file(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for (host_port, (algo, b64)) in &self.entries {
            out.push_str(&format!("{host_port} {algo} {b64}\n"));
        }
        fs::write(&self.path, out).with_context(|| format!("rewriting {:?}", self.path))?;
        Ok(())
    }
}

fn fingerprint_of_base64(b64: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    data_encoding::BASE64_NOPAD.encode(&hasher.finalize())
}

/// Blocking interactive prompt ("yes"/"no") describing a new or changed
/// host key, styled after OpenSSH's own prompt. Must be called via
/// `tokio::task::spawn_blocking` from async code (see `ssh_engine::mod`)
/// since it does a synchronous `stdin` read.
pub fn prompt_accept_new_key(host_port: &str, algo: &str, fingerprint: &str) -> bool {
    println!();
    println!("The authenticity of host '{host_port}' can't be established.");
    println!("{algo} key fingerprint is SHA256:{fingerprint}.");
    print!("Are you sure you want to continue connecting (yes/no)? ");
    let _ = std::io::stdout().flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "yes" | "y")
}

/// Blocking prompt shown for a *mismatched* key -- deliberately does not
/// offer a "trust anyway" option inline; the wording matches the severity
/// real SSH clients use, since this is the actual MITM-detection moment.
pub fn warn_mismatched_key(
    host_port: &str,
    stored_algo: &str,
    stored_fingerprint: &str,
    new_fingerprint: &str,
) {
    eprintln!();
    eprintln!("@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@");
    eprintln!("@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @");
    eprintln!("@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@");
    eprintln!("The {stored_algo} host key for {host_port} has changed.");
    eprintln!("It is also possible a MITM attack is happening.");
    eprintln!("Previously trusted fingerprint: SHA256:{stored_fingerprint}");
    eprintln!("Offered fingerprint now:        SHA256:{new_fingerprint}");
    eprintln!("Connection refused. If this change is expected (e.g. the server was");
    eprintln!("reinstalled), remove the stale entry from known_hosts and reconnect.");
    eprintln!();
}
