mod cli;
mod renderer;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use docgen::{fetcher, parser};

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
            let parsed = parser::parse_specs(specs, &k8s_version)?;
            println!(
                "Parsed {} resources ({} common definitions), rendering HTML...",
                parsed.resources.len(),
                parsed.common_defs.len()
            );
            renderer::render(
                &parsed.resources,
                &parsed.common_defs,
                &out,
                &base_url,
                is_latest,
                &renderer::TypeMaps {
                    classifications: &parsed.classifications,
                    simple_types: &parsed.simple_types,
                    complex_types: &parsed.complex_types,
                },
            )?;
        }
    }
    Ok(())
}
