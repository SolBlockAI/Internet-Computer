use std::{
    collections::HashMap,
    io::ErrorKind,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Error};
use async_trait::async_trait;
use axum::{
    body::Body,
    handler::Handler,
    http::{Request, Response, StatusCode},
    routing::get,
    Extension, Router,
};
use clap::{ArgEnum, Parser};
use futures::future::TryFutureExt;
use mockall::automock;
use nix::{
    sys::signal::{kill as send_signal, Signal},
    unistd::Pid,
};
use opentelemetry::{metrics::MeterProvider as _, sdk::metrics::MeterProvider};
use opentelemetry_prometheus::exporter;
use prometheus::{labels, Encoder, Registry, TextEncoder};
use rsa::{pkcs8::DecodePrivateKey, RsaPrivateKey};
use serde::Deserialize;
use serde_json as json;
use tokio::{
    fs::{self, File},
    io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader},
    task,
};
use tracing::info;

mod metrics;
use metrics::{MetricParams, WithMetrics};

mod decode;
use decode::{Decode, Decoder, NopDecoder};

const SERVICE_NAME: &str = "denylist-updater";

const MINUTE: Duration = Duration::from_secs(60);

#[derive(Clone, ArgEnum)]
enum DecodeMode {
    Nop,
    Decrypt,
}

#[derive(Parser)]
#[clap(name = SERVICE_NAME)]
#[clap(author = "Boundary Node Team <boundary-nodes@dfinity.org>")]
struct Cli {
    #[clap(long, default_value = "http://localhost:8000/denylist.json")]
    remote_url: String,

    #[clap(long, arg_enum, default_value = "nop")]
    decode_mode: DecodeMode,

    #[clap(long, default_value = "key.pem")]
    private_key_path: PathBuf,

    #[clap(long, default_value = "/tmp/denylist.map")]
    local_path: PathBuf,

    #[clap(long, default_value = "/var/run/nginx.pid")]
    pid_path: PathBuf,

    #[clap(long, default_value = "127.0.0.1:9090")]
    metrics_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();

    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .finish(),
    )
    .expect("failed to set global subscriber");

    // Metrics
    let registry: Registry = Registry::new_custom(
        None,
        Some(labels! {"service".into() => SERVICE_NAME.into()}),
    )
    .unwrap();
    let exporter = exporter().with_registry(registry.clone()).build()?;
    let provider = MeterProvider::builder().with_reader(exporter).build();
    let meter = provider.meter(SERVICE_NAME);
    let metrics_handler = metrics_handler.layer(Extension(MetricsHandlerArgs { registry }));
    let metrics_router = Router::new().route("/metrics", get(metrics_handler));

    let http_client = reqwest::Client::builder().build()?;

    let decoder: Arc<dyn Decode> = match cli.decode_mode {
        DecodeMode::Nop => Arc::new(NopDecoder),
        DecodeMode::Decrypt => {
            let private_key_pem = std::fs::read_to_string(cli.private_key_path)?;
            let private_key = RsaPrivateKey::from_pkcs8_pem(&private_key_pem)?;
            Arc::new(Decoder::new(private_key))
        }
    };

    let remote_lister = RemoteLister::new(http_client, decoder, cli.remote_url.clone());
    let remote_lister = WithNormalize(remote_lister);
    let remote_lister = WithMetrics(
        remote_lister,
        MetricParams::new(&meter, SERVICE_NAME, "list_remote"),
    );

    let local_lister = LocalLister::new(cli.local_path.clone());
    let local_lister = WithRecover(local_lister);
    let local_lister = WithNormalize(local_lister);
    let local_lister = WithMetrics(
        local_lister,
        MetricParams::new(&meter, SERVICE_NAME, "list_local"),
    );

    let reloader = Reloader::new(cli.pid_path, Signal::SIGHUP);
    let reloader = WithMetrics(reloader, MetricParams::new(&meter, SERVICE_NAME, "reload"));

    let updater = Updater::new(cli.local_path.clone());
    let updater = WithReload(updater, reloader);
    let updater = WithMetrics(updater, MetricParams::new(&meter, SERVICE_NAME, "update"));

    let runner = Runner::new(remote_lister, local_lister, updater);
    let runner = WithMetrics(runner, MetricParams::new(&meter, SERVICE_NAME, "run"));
    let runner = WithThrottle(runner, ThrottleParams::new(1 * MINUTE));
    let mut runner = runner;

    info!(
        msg = format!("starting {SERVICE_NAME}").as_str(),
        metrics_addr = cli.metrics_addr.to_string().as_str(),
    );

    let _ = tokio::try_join!(
        task::spawn(async move {
            loop {
                let _ = runner.run().await;
            }
        }),
        task::spawn(
            axum::Server::bind(&cli.metrics_addr)
                .serve(metrics_router.into_make_service())
                .map_err(|err| anyhow!("server failed: {:?}", err))
        )
    )
    .context(format!("{SERVICE_NAME} failed to run"))?;

    Ok(())
}

#[derive(Clone)]
struct MetricsHandlerArgs {
    registry: Registry,
}

async fn metrics_handler(
    Extension(MetricsHandlerArgs { registry }): Extension<MetricsHandlerArgs>,
    _: Request<Body>,
) -> Response<Body> {
    let metric_families = registry.gather();

    let encoder = TextEncoder::new();

    let mut metrics_text = Vec::new();
    if encoder.encode(&metric_families, &mut metrics_text).is_err() {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Internal Server Error".into())
            .unwrap();
    };

    Response::builder()
        .status(200)
        .body(metrics_text.into())
        .unwrap()
}

#[derive(Debug, PartialEq, Deserialize, Clone)]
struct Entry {
    id: String,
    localities: Vec<String>,
}

#[automock]
#[async_trait]
trait List: Send + Sync {
    async fn list(&self) -> Result<Vec<Entry>, Error>;
}

struct LocalLister {
    local_path: PathBuf,
}

impl LocalLister {
    fn new(local_path: PathBuf) -> Self {
        Self { local_path }
    }
}

#[async_trait]
impl List for LocalLister {
    async fn list(&self) -> Result<Vec<Entry>, Error> {
        let f = File::open(self.local_path.clone())
            .await
            .context("failed to open file")?;

        let f = BufReader::new(f);

        let mut lines = f.lines();
        let mut entries = vec![];

        while let Some(line) = lines.next_line().await? {
            let line = line
                .trim_start_matches("\"~^")
                .trim_end_matches("$\" \"1\";");
            let mut line = line.split_whitespace();
            if let Some(id) = line.next() {
                let localities = match line.next() {
                    Some("1;") => Vec::default(),
                    Some(".*") => Vec::default(),
                    Some(locs) => {
                        let locs = locs.trim_start_matches('(').trim_end_matches(')');
                        locs.split('|').map(str::to_string).collect()
                    }
                    None => anyhow::bail!("Invalid format."),
                };
                entries.push(Entry {
                    id: id.to_string(),
                    localities,
                });
            }
        }

        Ok(entries)
    }
}

struct RemoteLister {
    http_client: reqwest::Client,
    decoder: Arc<dyn Decode>,
    remote_url: String,
}

impl RemoteLister {
    fn new(http_client: reqwest::Client, decoder: Arc<dyn Decode>, remote_url: String) -> Self {
        Self {
            http_client,
            decoder,
            remote_url,
        }
    }
}

#[async_trait]
impl List for RemoteLister {
    async fn list(&self) -> Result<Vec<Entry>, Error> {
        let request = self
            .http_client
            .request(reqwest::Method::GET, self.remote_url.clone())
            .build()
            .context("failed to build request")?;

        let response = self
            .http_client
            .execute(request)
            .await
            .context("request failed")?;

        if response.status() != reqwest::StatusCode::OK {
            return Err(anyhow!("request failed with status {}", response.status()));
        }

        let data = response
            .bytes()
            .await
            .context("failed to get response bytes")?
            .to_vec();

        let data = self
            .decoder
            .decode(data)
            .await
            .context("failed to decode response")?;

        #[derive(Deserialize)]
        struct Canister {
            localities: Option<Vec<String>>,
        }

        #[derive(Deserialize)]
        struct Response {
            canisters: HashMap<String, Canister>,
        }

        let entries =
            json::from_slice::<Response>(&data).context("failed to deserialize json response")?;

        // Convert response body to entries
        let mut entries: Vec<Entry> = entries
            .canisters
            .into_iter()
            .map(|(id, canister)| Entry {
                id,
                localities: canister.localities.unwrap_or_default(),
            })
            .collect();

        entries.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(entries)
    }
}

#[automock]
#[async_trait]
trait Update: Send + Sync {
    async fn update(&self, entries: Vec<Entry>) -> Result<(), Error>;
}

struct Updater {
    local_path: PathBuf,
}

impl Updater {
    fn new(local_path: PathBuf) -> Self {
        Self { local_path }
    }
}

#[async_trait]
impl Update for Updater {
    async fn update(&self, entries: Vec<Entry>) -> Result<(), Error> {
        let mut f = File::create(self.local_path.clone())
            .await
            .context("failed to create file")?;

        for entry in entries {
            let line = if entry.localities.is_empty() {
                format!("\"~^{} .*$\" \"1\";\n", entry.id)
            } else {
                format!(
                    "\"~^{} ({})$\" \"1\";\n",
                    entry.id,
                    entry.localities.join("|")
                )
            };

            f.write_all(line.as_bytes())
                .await
                .context("failed to write entry")?;
        }

        f.flush().await?;

        Ok(())
    }
}

#[async_trait]
trait Reload: Sync + Send {
    async fn reload(&self) -> Result<(), Error>;
}

struct Reloader {
    pid_path: PathBuf,
    signal: Signal,
}

impl Reloader {
    fn new(pid_path: PathBuf, signal: Signal) -> Self {
        Self { pid_path, signal }
    }
}

#[async_trait]
impl Reload for Reloader {
    async fn reload(&self) -> Result<(), Error> {
        let pid = fs::read_to_string(self.pid_path.clone())
            .await
            .context("failed to read pid file")?;
        let pid = pid.trim().parse::<i32>().context("failed to parse pid")?;
        let pid = Pid::from_raw(pid);

        send_signal(pid, self.signal)?;

        Ok(())
    }
}

struct WithReload<T, R: Reload>(T, R);

#[async_trait]
impl<T: Update, R: Reload> Update for WithReload<T, R> {
    async fn update(&self, entries: Vec<Entry>) -> Result<(), Error> {
        let out = self.0.update(entries).await?;
        self.1.reload().await?;
        Ok(out)
    }
}

#[async_trait]
trait Run: Send + Sync {
    async fn run(&mut self) -> Result<(), Error>;
}

struct Runner<RL, LL, U> {
    remote_lister: RL,
    local_lister: LL,
    updater: U,
}

impl<RL: List, LL: List, U: Update> Runner<RL, LL, U> {
    fn new(remote_lister: RL, local_lister: LL, updater: U) -> Self {
        Self {
            remote_lister,
            local_lister,
            updater,
        }
    }
}

#[async_trait]
impl<RL: List, LL: List, U: Update> Run for Runner<RL, LL, U> {
    async fn run(&mut self) -> Result<(), Error> {
        let remote_entries = self
            .remote_lister
            .list()
            .await
            .context("failed to list remote entries")?;

        let local_entries = self
            .local_lister
            .list()
            .await
            .context("failed to list local entries")?;

        if remote_entries != local_entries {
            self.updater
                .update(remote_entries)
                .await
                .context("failed to update entries")?;
        }

        Ok(())
    }
}

struct ThrottleParams {
    throttle_duration: Duration,
    next_time: Option<Instant>,
}

impl ThrottleParams {
    fn new(throttle_duration: Duration) -> Self {
        Self {
            throttle_duration,
            next_time: None,
        }
    }
}

struct WithThrottle<T>(T, ThrottleParams);

#[async_trait]
impl<T: Run + Send + Sync> Run for WithThrottle<T> {
    async fn run(&mut self) -> Result<(), Error> {
        let current_time = Instant::now();
        let next_time = self.1.next_time.unwrap_or(current_time);

        if next_time > current_time {
            tokio::time::sleep(next_time - current_time).await;
        }
        self.1.next_time = Some(Instant::now() + self.1.throttle_duration);

        self.0.run().await
    }
}

struct WithNormalize<T: List>(T);

#[async_trait]
impl<T: List> List for WithNormalize<T> {
    async fn list(&self) -> Result<Vec<Entry>, Error> {
        self.0
            .list()
            .await
            .map(|mut entries| {
                entries.sort_by(|a, b| a.id.cmp(&b.id));
                entries
            })
            .map(|mut entries| {
                entries.dedup_by(|a, b| a.id == b.id);
                entries
            })
    }
}

struct WithRecover<T: List>(T);

#[async_trait]
impl<T: List> List for WithRecover<T> {
    async fn list(&self) -> Result<Vec<Entry>, Error> {
        match self.0.list().await {
            Err(err) => match io_error_kind(&err) {
                Some(ErrorKind::NotFound) => Ok(vec![]),
                _ => Err(err),
            },
            Ok(entries) => Ok(entries),
        }
    }
}

fn io_error_kind(err: &Error) -> Option<ErrorKind> {
    for cause in err.chain() {
        if let Some(err) = cause.downcast_ref::<io::Error>() {
            return Some(err.kind());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    use mockall::predicate;

    #[tokio::test]
    async fn it_lists_locally() -> Result<(), Error> {
        use std::fs::File;
        use std::io::Write;
        use tempfile::tempdir;

        // Create route files

        struct TestCase {
            name: &'static str,
            denylist_map: &'static str,
            want: Vec<Entry>,
        }

        let test_cases = vec![
            TestCase {
                name: "legacy",
                denylist_map: "ID_1 1;\nID_2 1;\n",
                want: vec![
                    Entry {
                        id: "ID_1".to_string(),
                        localities: Vec::default(),
                    },
                    Entry {
                        id: "ID_2".to_string(),
                        localities: Vec::default(),
                    },
                ],
            },
            TestCase {
                name: "geoblocking",
                denylist_map: "\"~^ID_1 (CH|US)$\" \"1\";\n\"~^ID_2 .*$\" \"1\";",
                want: vec![
                    Entry {
                        id: "ID_1".to_string(),
                        localities: vec!["CH".to_string(), "US".to_string()],
                    },
                    Entry {
                        id: "ID_2".to_string(),
                        localities: Vec::default(),
                    },
                ],
            },
        ];

        for tc in test_cases {
            let local_dir = tempdir()?;

            let (name, content) = &("denylist.map", tc.denylist_map);

            let file_path = local_dir.path().join(name);
            let mut file = File::create(file_path.clone())?;
            writeln!(file, "{}", content)?;

            // Create local lister
            let lister = LocalLister::new(file_path.clone());

            let was = lister.list().await?;
            assert_eq!(was, tc.want, "Test case '{}' failed.\n", tc.name);
        }

        Ok(())
    }

    #[tokio::test]
    async fn it_lists_remotely() -> Result<(), Error> {
        use httptest::{matchers::*, responders::*, Expectation, Server};
        use serde_json::json;

        struct TestCase {
            name: &'static str,
            denylist_json: json::Value,
            want: Vec<Entry>,
        }

        let test_cases = vec![
            TestCase {
                name: "legacy",
                denylist_json: json!({
                  "$schema": "./schema.json",
                  "version": "1",
                  "canisters": {
                    "ID_1": {},
                    "ID_2": {}
                  }
                }),
                want: vec![
                    Entry {
                        id: "ID_1".to_string(),
                        localities: Vec::default(),
                    },
                    Entry {
                        id: "ID_2".to_string(),
                        localities: Vec::default(),
                    },
                ],
            },
            TestCase {
                name: "geo_blocking",
                denylist_json: json!({
                  "$schema": "./schema.json",
                  "version": "1",
                  "canisters": {
                    "ID_1": {"localities": ["CH", "US"]},
                    "ID_2": {"localities": []},
                    "ID_3": {},
                  }
                }),
                want: vec![
                    Entry {
                        id: "ID_1".to_string(),
                        localities: vec!["CH".to_string(), "US".to_string()],
                    },
                    Entry {
                        id: "ID_2".to_string(),
                        localities: Vec::default(),
                    },
                    Entry {
                        id: "ID_3".to_string(),
                        localities: Vec::default(),
                    },
                ],
            },
        ];

        for tc in test_cases {
            let server = Server::run();
            server.expect(
                Expectation::matching(request::method_path("GET", "/denylist.json"))
                    .respond_with(json_encoded(tc.denylist_json)),
            );

            // Create remote lister
            let lister = RemoteLister::new(
                reqwest::Client::builder().build()?, // http_client
                Arc::new(NopDecoder),                // decoder
                server.url_str("/denylist.json"),    // remote_url
            );

            let was = lister.list().await?;
            assert_eq!(was, tc.want, "Test case '{}' failed.\n", tc.name);
        }

        Ok(())
    }

    #[tokio::test]
    async fn it_updates() -> Result<(), Error> {
        use tempfile::tempdir;

        struct TestCase {
            name: &'static str,
            entries: Vec<Entry>,
            want: &'static str,
        }

        let test_cases = vec![
            TestCase {
                name: "US",
                entries: vec![Entry {
                    id: "ID_1".to_string(),
                    localities: vec!["US".to_string()],
                }],
                want: "\"~^ID_1 (US)$\" \"1\";\n",
            },
            TestCase {
                name: "CH US",
                entries: vec![Entry {
                    id: "ID_1".to_string(),
                    localities: vec!["CH".to_string(), "US".to_string()],
                }],
                want: "\"~^ID_1 (CH|US)$\" \"1\";\n",
            },
            TestCase {
                name: "global",
                entries: vec![Entry {
                    id: "ID_1".to_string(),
                    localities: Vec::default(),
                }],
                want: "\"~^ID_1 .*$\" \"1\";\n",
            },
        ];
        for tc in test_cases {
            let local_dir = tempdir()?;
            let file_path = local_dir.path().join("denylist.map");

            // Create local lister
            let updater = Updater::new(file_path.clone());
            updater.update(tc.entries).await?;

            let was = fs::read_to_string(file_path).await?;

            assert_eq!(was, tc.want, "Test case '{}' failed.\n", tc.name);
        }
        Ok(())
    }

    #[tokio::test]
    async fn it_runs_eq() -> Result<(), Error> {
        struct TestCase {
            local: Vec<Entry>,
            remote: Vec<Entry>,
        }

        let test_cases = vec![
            TestCase {
                local: vec![],
                remote: vec![],
            },
            TestCase {
                local: vec![Entry {
                    id: "ID_1".to_string(),
                    localities: Vec::from(["CH".to_string(), "US".to_string()]),
                }],
                remote: vec![Entry {
                    id: "ID_1".to_string(),
                    localities: Vec::from(["CH".to_string(), "US".to_string()]),
                }],
            },
        ];

        for tc in test_cases {
            let mut remote_lister = MockList::new();
            remote_lister
                .expect_list()
                .times(1)
                .returning(move || Ok(tc.local.clone()));

            let mut local_lister = MockList::new();
            local_lister
                .expect_list()
                .times(1)
                .returning(move || Ok(tc.remote.clone()));

            let mut updater = MockUpdate::new();
            updater.expect_update().times(0);

            let mut runner = Runner::new(remote_lister, local_lister, updater);
            let was = runner.run().await;
            was?
        }

        Ok(())
    }

    fn eq(a: &[Entry], b: &[Entry]) -> bool {
        if a.len() != b.len() {
            return false;
        }

        let (mut a, mut b) = (a.iter(), b.iter());
        while let (Some(a), Some(b)) = (a.next(), b.next()) {
            if a.id != b.id {
                return false;
            }
        }

        true
    }

    #[tokio::test]
    async fn it_runs_neq() -> Result<(), Error> {
        let mut remote_lister = MockList::new();
        remote_lister.expect_list().times(1).returning(|| {
            Ok(vec![Entry {
                id: "ID_1".to_string(),
                localities: Vec::from(["CODE_1".to_string()]),
            }])
        });

        let mut local_lister = MockList::new();
        local_lister.expect_list().times(1).returning(|| Ok(vec![]));

        let mut updater = MockUpdate::new();
        updater
            .expect_update()
            .times(1)
            .with(predicate::function(|entries: &Vec<Entry>| {
                eq(
                    entries,
                    &[Entry {
                        id: "ID_1".to_string(),
                        localities: Vec::from(["CODE_1".to_string()]),
                    }],
                )
            }))
            .returning(|_| Ok(()));

        let mut runner = Runner::new(remote_lister, local_lister, updater);
        runner.run().await?;

        Ok(())
    }
}
