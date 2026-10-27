#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[arg(long, global = true)]
        user_data_dir: Option<String>,
        #[command(subcommand)]
        command: Commands,
    }

    #[derive(clap::Subcommand)]
    enum Commands {
        #[command(subcommand)]
        AcpMux(acp_mux::Command),
    }

    let arguments = Cli::parse();
    if let Some(directory) = arguments.user_data_dir {
        paths::set_custom_data_dir(&directory);
    }
    let Commands::AcpMux(command) = arguments.command;
    acp_mux::run(command, env!("CARGO_PKG_VERSION"))
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("acp-mux is only supported on Unix")
}
