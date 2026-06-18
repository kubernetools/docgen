use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "docgen",
    about = "Generate Kubernetes API documentation from OpenAPI specs"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Generate {
        #[arg(
            short = 'k',
            long = "k8s-version",
            help = "Kubernetes minor version (e.g. v1.36)"
        )]
        k8s_version: String,
        #[arg(short, long, default_value = "./site")]
        out: PathBuf,
        #[arg(
            long,
            default_value = "https://kubernetools.com",
            help = "Base URL for canonical links and sitemap"
        )]
        base_url: String,
        #[arg(long, env = "GITHUB_TOKEN")]
        token: Option<String>,
        #[arg(
            long,
            help = "Mark this version as latest: write sitemap.xml and robots.txt with /docs/latest/ URLs"
        )]
        is_latest: bool,
    },
}
