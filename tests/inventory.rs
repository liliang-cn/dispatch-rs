use dispatch::{Config, Dispatch};
use std::io::Write;

/// Resolution of wildcards (against ssh_config) and groups (from dispatch toml),
/// plus literal pass-through — all offline, no SSH needed.
#[test]
fn resolves_wildcards_groups_and_literals() {
    let dir = std::env::temp_dir().join(format!("dispatch-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let ssh = dir.join("ssh_config");
    let mut f = std::fs::File::create(&ssh).unwrap();
    write!(
        f,
        "Host orange1\n  HostName 192.168.1.1\n\
         Host orange2\n  HostName 192.168.1.2\n\
         Host web01\n  HostName 10.0.0.1\n\
         Host *\n  User root\n"
    )
    .unwrap();

    let toml = dir.join("dispatch.toml");
    std::fs::write(&toml, "[groups]\nweb = [\"web01\", \"orange*\"]\n").unwrap();

    let cfg = Config {
        ssh_config_path: Some(ssh),
        config_path: Some(toml),
        ..Default::default()
    };
    let client = Dispatch::new(cfg).unwrap();

    // wildcard expands against ssh-config aliases (not the `Host *` catch-all)
    let mut hosts = client.resolve(&["orange*".into()]).unwrap();
    hosts.sort();
    assert_eq!(hosts, vec!["orange1".to_string(), "orange2".to_string()]);

    // group expands (and nested wildcard inside the group works), deduped
    let mut group = client.resolve(&["web".into()]).unwrap();
    group.sort();
    assert_eq!(
        group,
        vec![
            "orange1".to_string(),
            "orange2".to_string(),
            "web01".to_string()
        ]
    );

    // literal alias / raw IP pass through unchanged
    assert_eq!(
        client.resolve(&["10.9.9.9".into()]).unwrap(),
        vec!["10.9.9.9".to_string()]
    );

    // unknown pattern with no wildcard is treated literally
    assert_eq!(
        client.resolve(&["db-primary".into()]).unwrap(),
        vec!["db-primary".to_string()]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An empty pattern set surfaces a clear error rather than silently succeeding.
#[test]
fn no_hosts_is_an_error() {
    let cfg = Config {
        ssh_config_path: Some("/nonexistent/ssh_config".into()),
        config_path: Some("/nonexistent/dispatch.toml".into()),
        ..Default::default()
    };
    let client = Dispatch::new(cfg).unwrap();
    assert!(client.resolve(&[]).is_err());
}
