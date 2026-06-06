# dispatch

A small async Rust library for **SSH batch operations**: run commands and manage
files across many servers in parallel, reusing your existing `~/.ssh/config`.

It wraps the system `ssh`/`scp`, so host aliases, wildcard `Host` blocks,
`IdentityFile`, `known_hosts`, `ProxyJump` and `ControlMaster` all behave exactly
as your ssh client is already configured — no separate credential handling.

## Features

- **`exec`** — run a command on many hosts concurrently, collect per-host
  stdout/stderr/exit code; optional `env`, working `dir`, stdin `input`, and
  **live streaming** of output as it arrives.
- **`copy`** — push a local file/dir to each host (`scp`).
- **`fetch`** — pull a remote path from each host into `dest/<host>/`.
- **`update`** — like copy, but **skips hosts whose file already matches**
  (sha256), with optional **backup** (`<dest>.bak`) and file **mode**.
- Host patterns: ssh-config aliases, **wildcards** (`orange*`), **groups** from
  `~/.dispatch/config.toml`, or raw IPs.
- Concurrency cap, per-host timeouts, `all_success()` / `failed_hosts()`.

## Requirements

The `ssh` and `scp` binaries must be on `PATH`. Authentication, host keys and
connection options come from your ssh configuration.

## Install

```toml
[dependencies]
dispatch = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Usage

```rust
use dispatch::{Config, Dispatch, StreamType};
use std::time::Duration;

#[tokio::main]
async fn main() -> dispatch::Result<()> {
    let client = Dispatch::new(Config::default())?; // reads ~/.ssh/config

    // Run a command on hosts matched by aliases / wildcards / groups / IPs.
    let res = client
        .exec(["orange1", "orange*"], "uptime")
        .parallel(5)
        .timeout(Duration::from_secs(30))
        .run()
        .await?;

    for (host, r) in &res.hosts {
        if r.success {
            println!("[{host}] {}", r.stdout.trim());
        } else {
            eprintln!("[{host}] exit {} {}", r.exit_code, r.stderr.trim());
        }
    }
    println!("all ok = {}", res.all_success());

    // Stream output live.
    client
        .exec(["web"], "tail -n5 /var/log/app.log")
        .stream(|host, ty, data| {
            let s = String::from_utf8_lossy(data);
            if ty == StreamType::Stderr { eprint!("[{host}] {s}") } else { print!("[{host}] {s}") }
        })
        .run()
        .await?;

    // File operations.
    client.copy(["web"], "./app.conf", "/etc/app.conf").run().await?;
    client.fetch(["web"], "/var/log/app.log", "./logs").run().await?; // -> ./logs/<host>/
    client
        .update(["web"], "./app.conf", "/etc/app.conf")
        .backup(true)
        .mode(0o644)
        .run()
        .await?; // skips hosts already up to date

    Ok(())
}
```

## Host resolution

A pattern resolves in this order:

1. a **group** name in `~/.dispatch/config.toml`

   ```toml
   [groups]
   web = ["web01", "web02"]
   orange = ["orange*"]      # members may themselves be wildcards/groups
   ```
2. a **wildcard** (`*`, `?`, `[..]`) matched against `Host` aliases in `~/.ssh/config`
3. a **literal** ssh alias, hostname, or IP (passed through unchanged;
   `user@host` is fine)

## License

MIT
