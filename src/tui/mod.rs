//! Interactive terminal UI.
use std::path::PathBuf;
use crate::agent::Agent;
pub async fn run(_agent: Agent, _cwd: PathBuf) -> anyhow::Result<()> { Ok(()) }
