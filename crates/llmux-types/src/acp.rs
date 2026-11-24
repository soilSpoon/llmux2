//! The `acp_agent` provider: how one credential describes the agent CLI the
//! gateway spawns as a managed child and talks ACP JSON-RPC with on stdio.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// How the child process lives relative to requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AcpSessionMode {
    /// A fresh child per request, killed when the turn ends. The safe default:
    /// no state survives between requests, so a wedged or dirty session can
    /// never leak into the next turn.
    #[default]
    PerRequest,
    /// One long-running child multiplexing `session/new` sessions, for
    /// app-server style CLIs designed to persist. Turns serialize through one
    /// connection; a dead or timed-out child is dropped and lazily respawned.
    Persistent,
}

/// The process one `acp_agent` credential spawns. Holds no secrets in its
/// fields' names; `env` values may carry them, so the spec is never logged.
#[derive(Clone)]
pub struct AcpAgentSpec {
    /// The agent CLI executable.
    pub command: PathBuf,
    /// Arguments that put it into its ACP/JSON-RPC stdio mode.
    pub args: Vec<String>,
    /// Extra environment variables on top of the scrubbed sandbox environment.
    pub env: BTreeMap<String, String>,
    /// Working directory for the child and the ACP session; defaults to `home`.
    pub cwd: Option<PathBuf>,
    /// Sandbox home: `HOME` and the XDG dirs point here so the CLI reads its
    /// own config/credentials from this directory.
    pub home: PathBuf,
    /// The inner bound for one turn: handshake (when fresh), `session/new`,
    /// and `session/prompt` together. The deployment timeout bounds the
    /// attempt on top of it.
    pub turn_timeout: Duration,
    pub mode: AcpSessionMode,
}

impl AcpAgentSpec {
    /// Where the child runs; defaults to the sandbox home.
    pub fn cwd(&self) -> &Path {
        self.cwd.as_deref().unwrap_or(&self.home)
    }
}
