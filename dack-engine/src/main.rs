//! Binary entry. Thin: parse the CLI and dispatch into the library. The harness loop
//! and every operator command live in [`dack::cli`].

use clap::Parser;

use dack::cli::{self, Cli};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli::dispatch(cli).await?;
    Ok(())
}
