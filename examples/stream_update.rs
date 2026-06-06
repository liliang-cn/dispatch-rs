//! Live test of streaming exec + update (skip/backup).
//! `cargo run --example stream_update -- liliang@192.168.123.214 ...`

use dispatch::{Config, Dispatch, StreamType};

#[tokio::main]
async fn main() -> dispatch::Result<()> {
    let hosts: Vec<String> = std::env::args().skip(1).collect();
    let client = Dispatch::new(Config::default())?;

    // --- streaming exec: print chunks as they arrive ---
    println!("== streaming ==");
    let res = client
        .exec(
            hosts.clone(),
            "for i in 1 2 3; do echo line-$i; sleep 0.3; done; echo oops 1>&2",
        )
        .stream(|host, ty, data| {
            let tag = if ty == StreamType::Stderr {
                "ERR"
            } else {
                "out"
            };
            print!("[{host}/{tag}] {}", String::from_utf8_lossy(data));
        })
        .run()
        .await?;
    println!("stream all_success={}", res.all_success());

    // --- update: first run writes, second run skips (unchanged) ---
    let local = std::env::temp_dir().join("dispatch-update.txt");
    std::fs::write(&local, "v1 content\n")?;

    println!("\n== update #1 (should write) ==");
    let u1 = client
        .update(hosts.clone(), &local, "/tmp/dispatch-update.txt")
        .backup(true)
        .mode(0o644)
        .run()
        .await?;
    for (h, r) in &u1.hosts {
        println!(
            "  [{h}] skipped={} bytes={} ok={}",
            r.skipped, r.bytes_copied, r.success
        );
    }

    println!("== update #2 (same content -> should skip) ==");
    let u2 = client
        .update(hosts, &local, "/tmp/dispatch-update.txt")
        .run()
        .await?;
    for (h, r) in &u2.hosts {
        println!(
            "  [{h}] skipped={} bytes={} ok={}",
            r.skipped, r.bytes_copied, r.success
        );
    }
    println!("skipped hosts on #2: {:?}", u2.skipped_hosts());
    Ok(())
}
