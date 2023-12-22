use anyhow::Result;
use clap::{Parser, Subcommand};
use k8s_openapi::chrono::Duration;
use tracing_subscriber::EnvFilter;

use k8s::tnet::TNet;

fn parse_duration(arg: &str) -> Result<Duration, std::num::ParseIntError> {
    let seconds = arg.parse()?;
    Ok(Duration::seconds(seconds))
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a testnet
    Create {
        /// Testnet name
        #[arg(short, long)]
        name: String,
        /// Testnet version
        #[arg(short, long)]
        version: String,
        /// Initialize a testnet
        #[arg(long)]
        init: bool,
        /// Use a zero version within testnet
        #[arg(long)]
        use_zero_version: bool,
        /// NNS subnet size
        #[arg(long)]
        nns: usize,
        /// APP subnet size
        #[arg(long)]
        app: usize,
        /// TTL in seconds
        #[arg(long)]
        #[arg(value_parser = parse_duration)]
        ttl: Option<Duration>,
    },
    /// Delete a testnet
    Delete {
        /// Testnet index
        #[arg(short, long)]
        index: u32,
    },
    /// List all testnets
    List {},
    /// Start the testnet
    Start {
        #[arg(short, long)]
        index: u32,
    },
    /// Stop the testnet
    Stop {
        #[arg(short, long)]
        index: u32,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    match &cli.command {
        Some(Commands::Create {
            name,
            version,
            init,
            use_zero_version,
            nns,
            app,
            ttl,
        }) => {
            let mut tnet = TNet::new(name)?
                .version(version)
                .use_zero_version(*use_zero_version)
                .init(*init)
                .topology(*nns, *app);
            if let Some(ttl) = ttl {
                tnet = tnet.ttl(*ttl)?;
            }
            tnet.create().await?;
        }
        Some(Commands::Delete { index }) => {
            TNet::delete(*index).await?;
        }
        Some(Commands::List {}) => {
            let list = TNet::list().await?;
            if list.is_empty() {
                println!("No resources found");
            } else {
                println!(" {:>10}     NAME", "ID");
                for (id, name) in list {
                    println!(" {:>10}  ⎈  {}", id, name);
                }
            }
        }
        Some(Commands::Start { index }) => {
            TNet::start(*index).await?;
        }
        Some(Commands::Stop { index }) => {
            TNet::stop(*index).await?;
        }
        None => {}
    }

    Ok(())
}
