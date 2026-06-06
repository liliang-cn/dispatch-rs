//! Live copy+fetch test: push a temp file to /tmp on each host, then fetch it back.
//! `cargo run --example xfer -- liliang@192.168.123.214 ...`

use dispatch::{Config, Dispatch};

#[tokio::main]
async fn main() -> dispatch::Result<()> {
    let hosts: Vec<String> = std::env::args().skip(1).collect();
    let client = Dispatch::new(Config::default())?;

    let local = std::env::temp_dir().join("dispatch-xfer.txt");
    std::fs::write(&local, "hello from dispatch-rs\n")?;

    let up = client
        .copy(hosts.clone(), &local, "/tmp/dispatch-xfer.txt")
        .run()
        .await?;
    println!("copy all_success={} {:?}", up.all_success(), up.failed_hosts());

    let out = std::env::temp_dir().join("dispatch-fetch");
    let _ = std::fs::remove_dir_all(&out);
    let down = client
        .fetch(hosts, "/tmp/dispatch-xfer.txt", &out)
        .run()
        .await?;
    println!("fetch all_success={} {:?}", down.all_success(), down.failed_hosts());
    for (host, r) in &down.hosts {
        println!("  fetched [{host}] -> {} (ok={})", r.dest, r.success);
    }
    Ok(())
}
