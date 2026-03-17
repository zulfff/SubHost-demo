use clap::{Parser, Subcommand};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::{Duration, Instant};
use hdrhistogram::Histogram;

#[derive(Parser)]
#[command(name = "subhost-bench")]
#[command(about = "Benchmark tool for Subhost Web3")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    
    #[arg(short, long, default_value = "http://localhost:8545")]
    endpoint: String,
    
    #[arg(short, long, default_value = "10")]
    duration_secs: u64,
    
    #[arg(short, long, default_value = "100")]
    concurrency: usize,
}

#[derive(Subcommand)]
enum Commands {
    Tps,
    Latency,
    Load,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    
    tracing_subscriber::fmt::init();
    
    match cli.command {
        Commands::Tps => run_tps_benchmark(&cli.endpoint, cli.duration_secs, cli.concurrency).await,
        Commands::Latency => run_latency_benchmark(&cli.endpoint, cli.duration_secs, cli.concurrency).await,
        Commands::Load => run_load_test(&cli.endpoint, cli.duration_secs, cli.concurrency).await,
    }
}

async fn run_tps_benchmark(endpoint: &str, duration_secs: u64, concurrency: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!("Running TPS benchmark...");
    println!("Endpoint: {}", endpoint);
    println!("Duration: {}s", duration_secs);
    println!("Concurrency: {}", concurrency);
    
    let total_requests = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let duration = Duration::from_secs(duration_secs);
    
    let mut handles = vec![];
    
    for _ in 0..concurrency {
        let counter = total_requests.clone();
        let handle = tokio::spawn(async move {
            let client = reqwest::Client::new();
            loop {
                if start.elapsed() >= duration {
                    break;
                }
                
                let _ = client
                    .post("http://localhost:8545")
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "eth_chainId",
                        "params": [],
                        "id": 1
                    }))
                    .send()
                    .await;
                
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        handles.push(handle);
    }
    
    for handle in handles {
        handle.await?;
    }
    
    let total = total_requests.load(Ordering::Relaxed);
    let elapsed = start.elapsed().as_secs_f64();
    let tps = total as f64 / elapsed;
    
    println!("\nResults:");
    println!("  Total requests: {}", total);
    println!("  Duration: {:.2}s", elapsed);
    println!("  TPS: {:.2}", tps);
    
    Ok(())
}

async fn run_latency_benchmark(endpoint: &str, duration_secs: u64, concurrency: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!("Running latency benchmark...");
    
    let mut histogram = Histogram::<u64>::new(3)?;
    let start = Instant::now();
    let duration = Duration::from_secs(duration_secs);
    let histogram = Arc::new(std::sync::Mutex::new(histogram));
    
    let mut handles = vec![];
    
    for _ in 0..concurrency {
        let hist = histogram.clone();
        let handle = tokio::spawn(async move {
            let client = reqwest::Client::new();
            loop {
                if start.elapsed() >= duration {
                    break;
                }
                
                let req_start = Instant::now();
                let _ = client
                    .post("http://localhost:8545")
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "eth_chainId",
                        "params": [],
                        "id": 1
                    }))
                    .send()
                    .await;
                let latency = req_start.elapsed().as_micros() as u64;
                
                if let Ok(mut h) = hist.lock() {
                    let _ = h.record(latency);
                }
            }
        });
        handles.push(handle);
    }
    
    for handle in handles {
        handle.await?;
    }
    
    let hist = histogram.lock().unwrap();
    println!("\nLatency Results:");
    println!("  Min: {} micros", hist.min());
    println!("  Max: {} micros", hist.max());
    println!("  Mean: {:.2} micros", hist.mean());
    println!("  P50: {} micros", hist.value_at_percentile(50.0));
    println!("  P95: {} micros", hist.value_at_percentile(95.0));
    println!("  P99: {} micros", hist.value_at_percentile(99.0));
    
    Ok(())
}

async fn run_load_test(endpoint: &str, duration_secs: u64, concurrency: usize) -> Result<(), Box<dyn std::error::Error>> {
    println!("Running load test...");
    println!("This will simulate realistic transaction load");
    
    let start = Instant::now();
    let duration = Duration::from_secs(duration_secs);
    
    let mut handles = vec![];
    
    for i in 0..concurrency {
        let handle = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let mut counter = 0u64;
            
            loop {
                if start.elapsed() >= duration {
                    break;
                }
                
                let delay = Duration::from_millis(100 + (i as u64 % 50));
                tokio::time::sleep(delay).await;
                
                let _ = client
                    .post("http://localhost:8545")
                    .json(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "eth_getBalance",
                        "params": [format!("0x{:040x}", counter), "latest"],
                        "id": counter
                    }))
                    .send()
                    .await;
                
                counter += 1;
            }
            
            counter
        });
        handles.push(handle);
    }
    
    let mut total = 0u64;
    for handle in handles {
        total += handle.await?;
    }
    
    let elapsed = start.elapsed().as_secs_f64();
    println!("\nLoad Test Results:");
    println!("  Total requests: {}", total);
    println!("  Duration: {:.2}s", elapsed);
    println!("  Average throughput: {:.2} req/s", total as f64 / elapsed);
    
    Ok(())
}
