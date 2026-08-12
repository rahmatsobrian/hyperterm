//! SSH-Agent authentication.
//!
//! `russh-keys`' built-in `AgentClient::connect_env()` only actually works
//! on Unix (it connects to the `SSH_AUTH_SOCK` Unix domain socket); on
//! every other platform the crate's own implementation is a stub that
//! unconditionally returns `Error::AgentFailure`. Since HyperTerm's primary
//! target is Windows, that stub is not good enough to call this feature
//! "implemented" -- so this module adds two real Windows transports,
//! tried in order:
//!
//! 1. The standard named pipe (`\\.\pipe\openssh-ssh-agent`) exposed by
//!    Windows' built-in "OpenSSH Authentication Agent" service (the same
//!    one `ssh.exe` and Git for Windows talk to).
//! 2. PuTTY's **Pageant**, via its `WM_COPYDATA` + shared-memory protocol
//!    (see the `pageant` submodule), if the named pipe isn't available.
//!
//! ## Honesty note for reviewers
//! The Windows code paths in this module cannot be exercised in the
//! sandbox this project was developed in (Linux, no Windows agent service
//! or Pageant process to connect to) and GitHub Actions' `windows-latest`
//! runners don't have either running, so CI validates that this code
//! *compiles*, not that it successfully authenticates against a live
//! agent. Please test against a real agent before relying on this in
//! production. See CONTRIBUTING.md.

use anyhow::{Context, Result};
use russh::client::Handle;
use russh::keys::agent::client::AgentClient;
use russh::keys::key::PublicKey;
use tokio::io::{AsyncRead, AsyncWrite};

use super::ClientHandler;

#[cfg(windows)]
#[path = "pageant.rs"]
pub mod pageant;

/// Tries every identity offered by the running SSH agent against the
/// server, in the order the agent reports them (matching OpenSSH client
/// behavior), returning `Ok(true)` on the first one the server accepts.
pub(crate) async fn authenticate(handle: &mut Handle<ClientHandler>, username: &str) -> Result<bool> {
    #[cfg(unix)]
    {
        let agent = AgentClient::connect_env()
            .await
            .context("connecting to SSH agent via $SSH_AUTH_SOCK (is ssh-agent running?)")?;
        try_all_identities(handle, username, agent).await
    }

    #[cfg(windows)]
    {
        match connect_windows_agent_pipe().await {
            Ok(agent) => {
                tracing::info!(target: "hyperterm::ssh_engine::agent", "connected via OpenSSH agent named pipe");
                try_all_identities(handle, username, agent).await
            }
            Err(pipe_err) => {
                tracing::info!(
                    target: "hyperterm::ssh_engine::agent",
                    "OpenSSH agent named pipe unavailable ({pipe_err}), trying Pageant"
                );
                let agent = pageant::connect()
                    .await
                    .context("connecting to either the OpenSSH agent named pipe or Pageant -- is an SSH agent running?")?;
                tracing::info!(target: "hyperterm::ssh_engine::agent", "connected via Pageant");
                try_all_identities(handle, username, agent).await
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!("ssh-agent authentication is not supported on this platform")
    }
}

#[cfg(windows)]
async fn connect_windows_agent_pipe(
) -> std::io::Result<AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>> {
    use std::time::Duration;
    use tokio::net::windows::named_pipe::ClientOptions;

    // Standard pipe name exposed by the Windows "OpenSSH Authentication
    // Agent" service (Settings -> Optional Features -> OpenSSH Client, then
    // `Set-Service ssh-agent -StartupType Automatic; Start-Service ssh-agent`).
    const PIPE_NAME: &str = r"\\.\pipe\openssh-ssh-agent";

    // Named pipes on Windows can report ERROR_PIPE_BUSY transiently if
    // another client is mid-handshake; retry briefly like the standard
    // Win32 named-pipe client pattern instead of failing immediately.
    let mut attempts = 0;
    loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(client) => return Ok(AgentClient::connect(client)),
            Err(e) if e.raw_os_error() == Some(231) && attempts < 5 => {
                // ERROR_PIPE_BUSY
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn try_all_identities<S>(
    handle: &mut Handle<ClientHandler>,
    username: &str,
    mut agent: AgentClient<S>,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let identities: Vec<PublicKey> = agent
        .request_identities()
        .await
        .context("listing identities from SSH agent")?;

    if identities.is_empty() {
        tracing::warn!(
            target: "hyperterm::ssh_engine::agent",
            "SSH agent is running but has no identities loaded (try `ssh-add` / `Add-Key`)"
        );
        return Ok(false);
    }

    tracing::info!(
        target: "hyperterm::ssh_engine::agent",
        "SSH agent offered {} identit{}",
        identities.len(),
        if identities.len() == 1 { "y" } else { "ies" }
    );

    for key in identities {
        let fp = key.fingerprint();
        let (returned_agent, result) =
            handle.authenticate_future(username, key, agent).await;
        agent = returned_agent;
        match result {
            Ok(true) => {
                tracing::info!(
                    target: "hyperterm::ssh_engine::agent",
                    "authenticated as '{username}' using agent identity {fp}"
                );
                return Ok(true);
            }
            Ok(false) => {
                tracing::debug!(
                    target: "hyperterm::ssh_engine::agent",
                    "server rejected agent identity {fp}, trying next"
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "hyperterm::ssh_engine::agent",
                    "agent signing error for identity {fp}: {e}, trying next"
                );
            }
        }
    }

    Ok(false)
}
