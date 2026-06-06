//! Live smoke test: `cargo run --example smoke -- liliang@192.168.123.214 ...`
//! Runs `hostname` + `uptime` on each host concurrently and prints results.

use dispatch::{Config, Dispatch};
use std::time::Duration;

#[tokio::main]
async fn main() -> dispatch::Result<()> {
    let hosts: Vec<String> = std::env::args().skip(1).collect();
    if hosts.is_empty() {
        eprintln!("usage: smoke <host> [host...]");
        std::process::exit(2);
    }

    let client = Dispatch::new(Config::default())?;
    let res = client
        .exec(hosts, "hostname; uptime")
        .parallel(5)
        .timeout(Duration::from_secs(15))
        .run()
        .await?;

    for (host, r) in &res.hosts {
        if r.success {
            println!("[{host}] OK\n{}", r.stdout.trim());
        } else {
            println!(
                "[{host}] FAIL exit={} err={:?}\n{}",
                r.exit_code,
                r.error,
                r.stderr.trim()
            );
        }
    }
    println!("\nall_success = {}", res.all_success());
    if !res.all_success() {
        println!("failed = {:?}", res.failed_hosts());
    }
    Ok(())
}
