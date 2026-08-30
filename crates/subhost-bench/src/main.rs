//! Load and latency benchmark client for a Subhost JSON-RPC endpoint.
//!
//! This measures a live endpoint. It reports request failures separately from
//! successes so a run against a dead node cannot be mistaken for throughput.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use hdrhistogram::Histogram;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subhost_telemetry::{TelemetryConfig, Verbosity};
use tokio::time::Instant;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:8545";

#[derive(Parser)]
#[command(name = "subhost-bench", about = "Benchmark a Subhost RPC endpoint", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, default_value = DEFAULT_ENDPOINT, global = true)]
    endpoint: String,

    /// How long to run, in seconds.
    #[arg(short, long, default_value_t = 10, global = true)]
    duration_secs: u64,

    /// Number of concurrent workers.
    #[arg(short, long, default_value_t = 32, global = true)]
    concurrency: usize,

    /// Per-request timeout, in seconds.
    #[arg(long, default_value_t = 10, global = true)]
    timeout_secs: u64,
}

#[derive(Subcommand, Clone, Copy)]
enum Commands {
    /// Maximum request throughput using `eth_chainId`.
    Tps,
    /// Latency distribution using `eth_chainId`.
    Latency,
    /// Paced read load using `eth_getBalance`.
    Load {
        /// Delay between requests per worker, in milliseconds.
        #[arg(long, default_value_t = 100)]
        pace_ms: u64,
    },
}

/// Aggregate outcome of a run.
#[derive(Debug, Default)]
struct Outcome {
    successes: u64,
    failures: u64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    subhost_telemetry::init_or_warn(TelemetryConfig::new(Verbosity::Normal));
    validate(&cli)?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(run(cli))
}

fn validate(cli: &Cli) -> Result<()> {
    if cli.concurrency == 0 {
        bail!("concurrency must be greater than zero");
    }
    if cli.duration_secs == 0 {
        bail!("duration must be greater than zero");
    }
    if cli.timeout_secs == 0 {
        bail!("timeout must be greater than zero");
    }
    if !cli.endpoint.starts_with("http://") && !cli.endpoint.starts_with("https://") {
        bail!("endpoint must be an http(s) URL");
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(cli.timeout_secs))
        .build()
        .context("cannot build the HTTP client")?;

    // Fail fast on an unreachable endpoint instead of reporting zero throughput.
    probe(&client, &cli.endpoint).await?;

    println!("endpoint:    {}", cli.endpoint);
    println!("duration:    {}s", cli.duration_secs);
    println!("concurrency: {}", cli.concurrency);

    match cli.command {
        Commands::Tps => throughput(&cli, &client).await,
        Commands::Latency => latency(&cli, &client).await,
        Commands::Load { pace_ms } => load(&cli, &client, pace_ms).await,
    }
}

/// Confirm the endpoint answers before measuring it.
async fn probe(client: &reqwest::Client, endpoint: &str) -> Result<()> {
    let response = request(client, endpoint, "eth_chainId", serde_json::json!([]))
        .await
        .with_context(|| format!("cannot reach the endpoint at {endpoint}"))?;
    if response.get("result").is_none() {
        bail!("endpoint {endpoint} did not answer eth_chainId with a result");
    }
    Ok(())
}

async fn throughput(cli: &Cli, client: &reqwest::Client) -> Result<()> {
    let successes = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(cli.duration_secs);
    let started = Instant::now();

    let mut workers = Vec::with_capacity(cli.concurrency);
    for _ in 0..cli.concurrency {
        let (client, endpoint) = (client.clone(), cli.endpoint.clone());
        let (successes, failures) = (successes.clone(), failures.clone());
        workers.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                match request(&client, &endpoint, "eth_chainId", serde_json::json!([])).await {
                    Ok(_) => successes.fetch_add(1, Ordering::Relaxed),
                    Err(_) => failures.fetch_add(1, Ordering::Relaxed),
                };
            }
        }));
    }
    for worker in workers {
        worker.await.context("a benchmark worker panicked")?;
    }

    report(
        "Throughput",
        started.elapsed(),
        Outcome {
            successes: successes.load(Ordering::Relaxed),
            failures: failures.load(Ordering::Relaxed),
        },
    );
    Ok(())
}

async fn latency(cli: &Cli, client: &reqwest::Client) -> Result<()> {
    // 1 µs to 60 s at three significant figures.
    let histogram = Arc::new(Mutex::new(
        Histogram::<u64>::new_with_bounds(1, 60_000_000, 3)
            .context("cannot build the histogram")?,
    ));
    let failures = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(cli.duration_secs);
    let started = Instant::now();

    let mut workers = Vec::with_capacity(cli.concurrency);
    for _ in 0..cli.concurrency {
        let (client, endpoint) = (client.clone(), cli.endpoint.clone());
        let (histogram, failures) = (histogram.clone(), failures.clone());
        workers.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let request_start = Instant::now();
                let outcome =
                    request(&client, &endpoint, "eth_chainId", serde_json::json!([])).await;
                let elapsed = request_start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                match outcome {
                    // Only successful requests enter the distribution; a timeout
                    // would otherwise masquerade as a slow but valid response.
                    Ok(_) => {
                        if let Ok(mut histogram) = histogram.lock() {
                            let _ = histogram.record(elapsed.max(1));
                        }
                    }
                    Err(_) => {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for worker in workers {
        worker.await.context("a benchmark worker panicked")?;
    }

    let histogram =
        histogram.lock().map_err(|_| anyhow::anyhow!("the latency histogram lock was poisoned"))?;
    let failures = failures.load(Ordering::Relaxed);
    report("Latency", started.elapsed(), Outcome { successes: histogram.len(), failures });
    if histogram.is_empty() {
        println!("  no successful requests to summarize");
        return Ok(());
    }
    println!("  min:  {} µs", histogram.min());
    println!("  mean: {:.2} µs", histogram.mean());
    println!("  p50:  {} µs", histogram.value_at_percentile(50.0));
    println!("  p95:  {} µs", histogram.value_at_percentile(95.0));
    println!("  p99:  {} µs", histogram.value_at_percentile(99.0));
    println!("  max:  {} µs", histogram.max());
    Ok(())
}

async fn load(cli: &Cli, client: &reqwest::Client, pace_ms: u64) -> Result<()> {
    let successes = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(cli.duration_secs);
    let started = Instant::now();

    let mut workers = Vec::with_capacity(cli.concurrency);
    for worker_index in 0..cli.concurrency {
        let (client, endpoint) = (client.clone(), cli.endpoint.clone());
        let (successes, failures) = (successes.clone(), failures.clone());
        // Stagger workers so they do not all fire on the same tick.
        let pace = Duration::from_millis(pace_ms + (worker_index as u64 % 50));
        workers.push(tokio::spawn(async move {
            let mut counter = 0u64;
            while Instant::now() < deadline {
                tokio::time::sleep(pace).await;
                let address = format!("0x{counter:040x}");
                match request(
                    &client,
                    &endpoint,
                    "eth_getBalance",
                    serde_json::json!([address, "latest"]),
                )
                .await
                {
                    Ok(_) => successes.fetch_add(1, Ordering::Relaxed),
                    Err(_) => failures.fetch_add(1, Ordering::Relaxed),
                };
                counter = counter.wrapping_add(1);
            }
        }));
    }
    for worker in workers {
        worker.await.context("a benchmark worker panicked")?;
    }

    report(
        "Load",
        started.elapsed(),
        Outcome {
            successes: successes.load(Ordering::Relaxed),
            failures: failures.load(Ordering::Relaxed),
        },
    );
    Ok(())
}

/// Issue one JSON-RPC call, treating a JSON-RPC error object as a failure.
async fn request(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1,
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    if let Some(error) = response.get("error") {
        bail!("{method} failed: {error}");
    }
    Ok(response)
}

fn report(label: &str, elapsed: Duration, outcome: Outcome) {
    let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
    let total = outcome.successes + outcome.failures;
    println!("\n{label} results:");
    println!("  elapsed:     {:.2}s", elapsed.as_secs_f64());
    println!("  successes:   {}", outcome.successes);
    println!("  failures:    {}", outcome.failures);
    println!("  throughput:  {:.2} successful req/s", outcome.successes as f64 / seconds);
    if total > 0 {
        println!("  error rate:  {:.2}%", outcome.failures as f64 / total as f64 * 100.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(args.iter().copied()).expect("arguments must parse")
    }

    #[test]
    fn cli_definition_has_no_argument_collisions() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_subcommand_parses_with_defaults() {
        for args in [
            &["subhost-bench", "tps"][..],
            &["subhost-bench", "latency"],
            &["subhost-bench", "load"],
            &["subhost-bench", "load", "--pace-ms", "50"],
            &["subhost-bench", "--endpoint", "http://127.0.0.1:9999", "tps"],
            &["subhost-bench", "-d", "5", "-c", "4", "latency"],
        ] {
            assert!(Cli::try_parse_from(args.iter().copied()).is_ok(), "{args:?}");
        }
        let parsed = cli(&["subhost-bench", "tps"]);
        assert_eq!(parsed.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(parsed.duration_secs, 10);
        assert_eq!(parsed.concurrency, 32);
        assert_eq!(parsed.timeout_secs, 10);
    }

    #[test]
    fn validation_rejects_degenerate_parameters() {
        assert!(validate(&cli(&["subhost-bench", "tps"])).is_ok());
        assert!(validate(&cli(&["subhost-bench", "-c", "0", "tps"])).is_err());
        assert!(validate(&cli(&["subhost-bench", "-d", "0", "tps"])).is_err());
        assert!(validate(&cli(&["subhost-bench", "--timeout-secs", "0", "tps"])).is_err());
        assert!(validate(&cli(&["subhost-bench", "--endpoint", "127.0.0.1:8545", "tps"])).is_err());
    }

    #[tokio::test]
    async fn probe_fails_on_an_unreachable_endpoint() {
        let client =
            reqwest::Client::builder().timeout(Duration::from_millis(250)).build().unwrap();
        // Port 1 has nothing listening.
        assert!(probe(&client, "http://127.0.0.1:1").await.is_err());
    }

    #[tokio::test]
    async fn a_json_rpc_error_object_counts_as_a_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer).await;
            let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let client = reqwest::Client::new();
        let result =
            request(&client, &format!("http://{addr}"), "eth_chainId", serde_json::json!([])).await;
        assert!(result.is_err(), "a JSON-RPC error must not count as success");

        server.await.unwrap();
    }

    #[test]
    fn report_handles_a_zero_request_run_without_dividing_by_zero() {
        // Must not panic on an empty run.
        report("Throughput", Duration::from_secs(0), Outcome::default());
        report("Throughput", Duration::from_secs(2), Outcome { successes: 10, failures: 10 });
    }
}
