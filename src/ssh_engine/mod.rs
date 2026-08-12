//! SSH Engine
//!
//! Wraps `russh` (pure-Rust SSH implementation, no OpenSSL/libssh2 native
//! dependency, which keeps Windows cross-compilation simple and the binary
//! self-contained) into a small interactive-shell-focused API:
//!
//!   - Password, public key (RSA / ED25519 / any `russh`-supported OpenSSH
//!     key format), and SSH-agent authentication.
//!   - PTY + interactive shell (not just one-shot `exec`).
//!   - Host key verification against a persisted `known_hosts`-equivalent
//!     store (see the `known_hosts` submodule), with configurable policy:
//!     interactive prompt (default), trust-on-first-use, or strict.

pub mod agent;
pub mod known_hosts;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::key::{self, KeyPair};
use russh::{ChannelId, ChannelMsg, Disconnect};

pub use known_hosts::HostKeyPolicy;
use known_hosts::{KnownHostsStore, Verdict};

#[derive(Debug, Clone)]
pub enum AuthMethod {
    Password(String),
    PrivateKeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    Agent,
}

#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub keepalive_interval: Duration,
    pub inactivity_timeout: Option<Duration>,
    pub host_key_policy: HostKeyPolicy,
    pub known_hosts_path: PathBuf,
}

/// Events surfaced to the terminal/render layer. Kept intentionally small
/// and decoded (no raw russh types) so `terminal_core`/`renderer` don't need
/// to depend on `russh` at all.
#[derive(Debug)]
pub enum SshEvent {
    Data(Vec<u8>),
    ExtendedData(Vec<u8>),
    ExitStatus(u32),
    Eof,
    Closed,
    Disconnected { reason: String },
}

pub(crate) struct ClientHandler {
    host_port: String,
    known_hosts: KnownHostsStore,
    policy: HostKeyPolicy,
}

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        match self.known_hosts.check(&self.host_port, server_public_key) {
            Verdict::Matches => {
                tracing::debug!(
                    target: "hyperterm::ssh_engine",
                    "host key for '{}' matches known_hosts entry",
                    self.host_port
                );
                Ok(true)
            }
            Verdict::Mismatch {
                stored_algo,
                stored_fingerprint,
            } => {
                // SECURITY: a changed key is refused unconditionally,
                // regardless of `self.policy`. This is intentional -- see
                // `known_hosts` module docs.
                tracing::error!(
                    target: "hyperterm::ssh_engine",
                    "REFUSING connection: host key for '{}' changed (stored {} fingerprint {}, offered {})",
                    self.host_port,
                    stored_algo,
                    stored_fingerprint,
                    server_public_key.fingerprint()
                );
                known_hosts::warn_mismatched_key(
                    &self.host_port,
                    &stored_algo,
                    &stored_fingerprint,
                    &server_public_key.fingerprint(),
                );
                Ok(false)
            }
            Verdict::New => {
                let accepted = match self.policy {
                    HostKeyPolicy::Strict => {
                        tracing::error!(
                            target: "hyperterm::ssh_engine",
                            "REFUSING connection: unknown host key for '{}' under Strict policy",
                            self.host_port
                        );
                        false
                    }
                    HostKeyPolicy::Tofu => {
                        tracing::warn!(
                            target: "hyperterm::ssh_engine",
                            "auto-trusting new host key for '{}' (fingerprint {}) under TOFU policy",
                            self.host_port,
                            server_public_key.fingerprint()
                        );
                        true
                    }
                    HostKeyPolicy::Prompt => {
                        let host_port = self.host_port.clone();
                        let algo = server_public_key.name().to_string();
                        let fingerprint = server_public_key.fingerprint();
                        tokio::task::spawn_blocking(move || {
                            known_hosts::prompt_accept_new_key(&host_port, &algo, &fingerprint)
                        })
                        .await
                        .unwrap_or(false)
                    }
                };
                if accepted {
                    if let Err(e) = self.known_hosts.trust(&self.host_port, server_public_key) {
                        tracing::error!(
                            target: "hyperterm::ssh_engine",
                            "accepted host key for '{}' but failed to persist it to known_hosts: {e}",
                            self.host_port
                        );
                        // Fail closed: if we can't remember the decision,
                        // don't silently proceed as if we had.
                        return Ok(false);
                    }
                } else {
                    tracing::warn!(
                        target: "hyperterm::ssh_engine",
                        "user declined host key for '{}', aborting connection",
                        self.host_port
                    );
                }
                Ok(accepted)
            }
        }
    }
}

pub struct SshSession {
    handle: Handle<ClientHandler>,
    channel: russh::Channel<client::Msg>,
}

impl SshSession {
    #[tracing::instrument(skip(params), fields(host = %params.host, port = params.port))]
    pub async fn connect(params: ConnectParams, cols: u32, rows: u32) -> Result<Self> {
        let config = client::Config {
            inactivity_timeout: params.inactivity_timeout,
            keepalive_interval: Some(params.keepalive_interval),
            keepalive_max: 3,
            ..Default::default()
        };
        let config = Arc::new(config);
        let known_hosts = KnownHostsStore::load(&params.known_hosts_path)?;
        let handler = ClientHandler {
            host_port: format!("{}:{}", params.host, params.port),
            known_hosts,
            policy: params.host_key_policy,
        };

        tracing::info!(target: "hyperterm::ssh_engine", "connecting to {}:{}", params.host, params.port);
        let mut handle = client::connect(config, (params.host.as_str(), params.port), handler)
            .await
            .with_context(|| {
                format!("TCP/KEX connect to {}:{} failed", params.host, params.port)
            })?;

        let authenticated = match &params.auth {
            AuthMethod::Password(pw) => {
                tracing::info!(target: "hyperterm::ssh_engine", "authenticating as '{}' via password", params.username);
                handle.authenticate_password(&params.username, pw).await?
            }
            AuthMethod::PrivateKeyFile { path, passphrase } => {
                tracing::info!(
                    target: "hyperterm::ssh_engine",
                    "authenticating as '{}' via private key {:?}",
                    params.username, path
                );
                let key_pair: KeyPair = russh::keys::load_secret_key(path, passphrase.as_deref())
                    .with_context(|| format!("loading private key {path:?}"))?;
                handle
                    .authenticate_publickey(&params.username, Arc::new(key_pair))
                    .await?
            }
            AuthMethod::Agent => {
                tracing::info!(target: "hyperterm::ssh_engine", "authenticating as '{}' via ssh-agent", params.username);
                agent::authenticate(&mut handle, &params.username)
                    .await
                    .context("ssh-agent authentication failed")?
            }
        };

        if !authenticated {
            bail!("SSH authentication failed for user '{}'", params.username);
        }
        tracing::info!(target: "hyperterm::ssh_engine", "authenticated as '{}'", params.username);

        let channel = handle
            .channel_open_session()
            .await
            .context("opening session channel")?;
        channel
            .request_pty(false, "xterm-256color", cols, rows, 0, 0, &[])
            .await
            .context("requesting PTY")?;
        channel
            .request_shell(true)
            .await
            .context("requesting shell")?;

        Ok(Self { handle, channel })
    }

    /// Waits for the next event from the SSH channel. Intended to be used
    /// as one arm of a `tokio::select!` alongside PTY input and render
    /// ticks in the main loop (see `main.rs`), so the SSH read path never
    /// blocks keyboard input or rendering. Returns `None` once the channel
    /// has fully closed.
    pub async fn next_event(&mut self) -> Option<SshEvent> {
        match self.channel.wait().await {
            Some(ChannelMsg::Data { data }) => Some(SshEvent::Data(data.to_vec())),
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                Some(SshEvent::ExtendedData(data.to_vec()))
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => Some(SshEvent::ExitStatus(exit_status)),
            Some(ChannelMsg::Eof) => Some(SshEvent::Eof),
            Some(ChannelMsg::Close) => Some(SshEvent::Closed),
            Some(_) => Some(SshEvent::Data(Vec::new())), // ignore unhandled variants without dropping the loop
            None => None,
        }
    }

    pub async fn send_input(&self, bytes: &[u8]) -> Result<()> {
        self.channel
            .data(bytes)
            .await
            .context("writing to SSH channel")?;
        Ok(())
    }

    pub async fn resize(&self, cols: u32, rows: u32) -> Result<()> {
        self.channel
            .window_change(cols, rows, 0, 0)
            .await
            .context("sending window-change")?;
        Ok(())
    }

    pub fn channel_id(&self) -> ChannelId {
        self.channel.id()
    }

    pub async fn close(&mut self) -> Result<()> {
        let _ = self.channel.eof().await;
        let _ = self.channel.close().await;
        self.handle
            .disconnect(Disconnect::ByApplication, "user closed session", "en")
            .await
            .ok();
        Ok(())
    }
}
