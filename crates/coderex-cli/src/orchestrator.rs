//! Startup orchestrator — four-phase initialization.

use anyhow::Result;
use crate::cli::Cli;

/// Phases of startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupPhase {
    /// Core initialization.
    Bootstrap,
    /// Load config files.
    LoadSettings,
    /// Load remote-managed overrides.
    MergeRemoteManagedSettings,
    /// Build the capability set.
    BuildCapabilities,
    /// Register slash commands.
    LoadCommands,
    /// Register skills.
    LoadSkills,
    /// Initialize plugins.
    InitPlugins,
    /// Reconcile marketplace state.
    ReconcileMarketplaces,
    /// Initialize MCP servers.
    InitMcp,
    /// Initialize memory subsystem.
    InitMemory,
    /// Initialize remote services.
    InitRemoteServices,
    /// All initialization complete.
    Ready,
}

/// Orchestrates the startup sequence.
pub struct StartupOrchestrator;

impl StartupOrchestrator {
    /// Create a new orchestrator.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Run the full startup sequence.
    pub async fn run(&self, _args: Cli) -> Result<()> {
        self.run_phase(StartupPhase::Bootstrap).await?;
        self.run_phase(StartupPhase::LoadSettings).await?;
        self.run_phase(StartupPhase::MergeRemoteManagedSettings).await?;
        self.run_phase(StartupPhase::BuildCapabilities).await?;
        self.run_phase(StartupPhase::LoadCommands).await?;
        self.run_phase(StartupPhase::LoadSkills).await?;
        self.run_phase(StartupPhase::InitPlugins).await?;
        self.run_phase(StartupPhase::ReconcileMarketplaces).await?;
        self.run_phase(StartupPhase::InitMcp).await?;
        self.run_phase(StartupPhase::InitMemory).await?;
        self.run_phase(StartupPhase::InitRemoteServices).await?;
        self.run_phase(StartupPhase::Ready).await?;

        tracing::info!("CodeRex ready");

        // TODO: enter REPL or execute command based on Cli args
        Ok(())
    }

    #[allow(clippy::unused_async)]
    async fn run_phase(&self, phase: StartupPhase) -> Result<()> {
        tracing::info!("startup phase: {phase:?}");
        // TODO: implement each phase
        Ok(())
    }
}
