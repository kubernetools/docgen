mod cli;
mod fetcher;
mod model;
mod parser;
mod renderer;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Generate {
            k8s_version,
            out,
            base_url,
            token,
            is_latest,
        } => {
            println!("Fetching Kubernetes {k8s_version} specs...");
            let specs = fetcher::fetch_specs(&k8s_version, token.as_deref()).await?;
            println!("Parsing {} spec files...", specs.len());
            let (resources, common_defs) = parser::parse_specs(specs, &k8s_version)?;
            println!(
                "Parsed {} resources ({} common definitions), rendering HTML...",
                resources.len(),
                common_defs.len()
            );
            renderer::render(&resources, &common_defs, &out, &base_url, is_latest)?;
        }
    }
    Ok(())
}
