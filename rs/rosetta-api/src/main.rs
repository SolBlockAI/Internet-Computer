use clap::Parser;
use ic_crypto_utils_threshold_sig_der::{
    parse_threshold_sig_key, parse_threshold_sig_key_from_der,
};
use ic_rosetta_api::request_handler::RosettaRequestHandler;
use ic_rosetta_api::rosetta_server::{RosettaApiServer, RosettaApiServerOpt};
use ic_rosetta_api::{ledger_client, DEFAULT_BLOCKCHAIN, DEFAULT_TOKEN_SYMBOL};
use ic_types::{CanisterId, PrincipalId};
use std::{path::Path, path::PathBuf, str::FromStr, sync::Arc};
use url::Url;

#[derive(Debug, Parser)]
#[clap(version)]
struct Opt {
    #[clap(short = 'a', long = "address", default_value = "0.0.0.0")]
    listen_address: String,
    /// The listen port of Rosetta. If not set then the port used will be 8081 unless --listen-port-file is
    /// defined in which case a random port is used.
    #[clap(short = 'p', long = "port")]
    listen_port: Option<u16>,
    /// File where the port will be written. Useful when the port is set to 0 because a random port will be picked.
    #[clap(short = 'P', long = "port-file")]
    listen_port_file: Option<PathBuf>,
    /// Id of the ICP ledger canister.
    #[clap(short = 'c', long = "canister-id")]
    ic_canister_id: Option<String>,
    #[clap(short = 't', long = "token-sybol")]
    token_symbol: Option<String>,
    /// Id of the governance canister to use for neuron management.
    #[clap(short = 'g', long = "governance-canister-id")]
    governance_canister_id: Option<String>,
    /// The URL of the replica to connect to.
    #[clap(long = "ic-url")]
    ic_url: Option<String>,
    #[clap(
        short = 'l',
        long = "log-config-file",
        default_value = "log_config.yml"
    )]
    log_config_file: PathBuf,
    #[clap(long = "root-key")]
    root_key: Option<PathBuf>,
    /// Supported options: sqlite, sqlite-in-memory
    #[clap(long = "store-type", default_value = "sqlite")]
    store_type: String,
    #[clap(long = "store-location", default_value = "./data")]
    store_location: PathBuf,
    #[clap(long = "store-max-blocks")]
    store_max_blocks: Option<u64>,
    #[clap(long = "exit-on-sync")]
    exit_on_sync: bool,
    #[clap(long = "offline")]
    offline: bool,
    #[clap(long = "mainnet", help = "Connect to the Internet Computer Mainnet")]
    mainnet: bool,
    /// The name of the blockchain reported in the network identifier.
    #[clap(long = "blockchain", default_value = DEFAULT_BLOCKCHAIN)]
    blockchain: String,
    #[clap(long = "not-whitelisted")]
    not_whitelisted: bool,
    #[clap(long = "expose-metrics")]
    expose_metrics: bool,
}

impl Opt {
    fn default_url(&self) -> Url {
        let url = if self.mainnet {
            "https://ic0.app"
        } else {
            "https://exchanges.testnet.dfinity.network"
        };
        Url::parse(url).unwrap()
    }

    fn ic_url(&self) -> Result<Url, String> {
        match self.ic_url.as_ref() {
            None => Ok(self.default_url()),
            Some(s) => Url::parse(s).map_err(|e| format!("Unable to parse --ic-url: {}", e)),
        }
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let opt = Opt::parse();

    if let Err(e) = log4rs::init_file(opt.log_config_file.as_path(), Default::default()) {
        panic!(
            "rosetta-api failed to load log configuration file: {}, error: {}. (current_dir is: {:?})",
            &opt.log_config_file.as_path().display(),
            e,
            std::env::current_dir()
        );
    }

    let pkg_name = env!("CARGO_PKG_NAME");
    let pkg_version = env!("CARGO_PKG_VERSION");
    log::info!("Starting {}, pkg_version: {}", pkg_name, pkg_version);
    let listen_port = match (opt.listen_port, &opt.listen_port_file) {
        (None, None) => 8081,
        (None, Some(_)) => 0, // random port
        (Some(p), _) => p,
    };
    log::info!("Listening on {}:{}", opt.listen_address, listen_port);
    let addr = format!("{}:{}", opt.listen_address, listen_port);
    let url = opt.ic_url().unwrap();

    let (root_key, canister_id, governance_canister_id) = if opt.mainnet {
        let root_key = match opt.root_key {
            Some(root_key_path) => parse_threshold_sig_key(root_key_path.as_path())?,
            None => {
                // The mainnet root key
                let root_key_text = r#"MIGCMB0GDSsGAQQBgtx8BQMBAgEGDCsGAQQBgtx8BQMCAQNhAIFMDm7HH6tYOwi9gTc8JVw8NxsuhIY8mKTx4It0I10U+12cDNVG2WhfkToMCyzFNBWDv0tDkuRn25bWW5u0y3FxEvhHLg1aTRRQX/10hLASkQkcX4e5iINGP5gJGguqrg=="#;
                let decoded = base64::decode(root_key_text).unwrap();
                parse_threshold_sig_key_from_der(&decoded).unwrap()
            }
        };

        let canister_id = match opt.ic_canister_id {
            Some(cid) => {
                CanisterId::unchecked_from_principal(PrincipalId::from_str(&cid[..]).unwrap())
            }
            None => ic_nns_constants::LEDGER_CANISTER_ID,
        };

        let governance_canister_id = match opt.governance_canister_id {
            Some(cid) => {
                CanisterId::unchecked_from_principal(PrincipalId::from_str(&cid[..]).unwrap())
            }
            None => ic_nns_constants::GOVERNANCE_CANISTER_ID,
        };

        (Some(root_key), canister_id, governance_canister_id)
    } else {
        let root_key = match opt.root_key {
            Some(root_key_path) => Some(parse_threshold_sig_key(root_key_path.as_path())?),
            None => {
                log::warn!("Data certificate will not be verified due to missing root key");
                None
            }
        };

        let canister_id = match opt.ic_canister_id {
            Some(cid) => {
                CanisterId::unchecked_from_principal(PrincipalId::from_str(&cid[..]).unwrap())
            }
            None => ic_nns_constants::LEDGER_CANISTER_ID,
        };

        let governance_canister_id = match opt.governance_canister_id {
            Some(cid) => {
                CanisterId::unchecked_from_principal(PrincipalId::from_str(&cid[..]).unwrap())
            }
            None => ic_nns_constants::GOVERNANCE_CANISTER_ID,
        };

        (root_key, canister_id, governance_canister_id)
    };

    let token_symbol = opt
        .token_symbol
        .unwrap_or_else(|| DEFAULT_TOKEN_SYMBOL.to_string());
    log::info!("Token symbol set to {}", token_symbol);

    let store_location: Option<&Path> = match opt.store_type.as_ref() {
        "sqlite" => Some(&opt.store_location),
        "sqlite-in-memory" | "in-memory" => {
            log::info!("Using in-memory block store");
            None
        }
        _ => {
            log::error!("Invalid store type. Expected sqlite or sqlite-in-memory.");
            panic!("Invalid store type");
        }
    };

    let Opt {
        store_max_blocks,
        offline,
        exit_on_sync,
        mainnet,
        not_whitelisted,
        expose_metrics,
        blockchain,
        ..
    } = opt;
    let client = ledger_client::LedgerClient::new(
        url,
        canister_id,
        token_symbol,
        governance_canister_id,
        store_location,
        store_max_blocks,
        offline,
        root_key,
    )
    .await
    .map_err(|e| {
        let msg = if mainnet && !not_whitelisted && e.is_internal_error_403() {
            ", You may not be whitelisted; please try running the Rosetta server again with the '--not_whitelisted' flag"
        } else {""};
        (e, msg)
    })
    .unwrap_or_else(|(e, is_403)| panic!("Failed to initialize ledger client{}: {:?}", is_403, e));

    let ledger = Arc::new(client);
    let req_handler = RosettaRequestHandler::new(blockchain, ledger.clone());

    log::info!("Network id: {:?}", req_handler.network_id());
    let serv = RosettaApiServer::new(
        ledger,
        req_handler,
        addr,
        opt.listen_port_file,
        expose_metrics,
    )
    .expect("Error creating RosettaApiServer");

    // actix server catches kill signals. After that we still need to stop our
    // server properly
    serv.run(RosettaApiServerOpt {
        exit_on_sync,
        offline,
        mainnet,
        not_whitelisted,
    })
    .await
    .unwrap();
    serv.stop().await;
    log::info!("Th-th-th-that's all folks!");
    Ok(())
}
