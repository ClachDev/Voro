//! A `voro.toml` carrying a `[costs]` table loads with one warning (DESIGN.md
//! §5), and every CLI verb prints it on stderr with a single prefix.

use std::process::Command;

#[test]
fn a_costs_table_prints_one_warning_with_one_prefix() {
    let root = tempfile::Builder::new()
        .prefix("voro-costs-")
        .tempdir()
        .unwrap()
        .keep();
    let config_home = root.join("config");
    std::fs::create_dir_all(config_home.join("voro")).unwrap();
    std::fs::write(config_home.join("voro/voro.toml"), "[costs]\ndo = 1.8\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_voro"))
        .arg("--db")
        .arg(root.join("voro.db"))
        .arg("list")
        .env("XDG_CONFIG_HOME", &config_home)
        .output()
        .expect("spawn voro");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");

    let warnings: Vec<&str> = stderr.lines().filter(|l| l.contains("[costs]")).collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert!(
        warnings[0].starts_with("warning: the [costs] table"),
        "{}",
        warnings[0]
    );
}
