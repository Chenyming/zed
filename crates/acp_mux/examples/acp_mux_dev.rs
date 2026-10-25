//! Runs acp-mux without building the whole remote server, for trying clients
//! (such as the web client or a mobile app) against a local daemon and a fake
//! agent. Accepts the same `acp-mux <command>` arguments as the remote server,
//! since the daemon is started by running the current executable that way.
//!
//! Set `HOME` to a scratch directory to keep its config, token, socket and
//! logs away from a real Zed installation.

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    enum Cli {
        #[command(subcommand)]
        AcpMux(acp_mux::Command),
    }

    let Cli::AcpMux(command) = Cli::parse();
    acp_mux::run(command, "dev")
}

#[cfg(not(unix))]
fn main() {}
