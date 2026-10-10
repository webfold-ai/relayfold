//! The real `relayfold-control` binary, as a subprocess: argument and environment handling, and
//! one signed round trip against the process it started (real clock, real socket).

mod common;

use common::*;
use relayfold_control::auth::{key_to_z32, public_key_of, Signer};
use servelet_client::Client;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_relayfold-control");

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Start the binary on `127.0.0.1:0` and read the real port from its first stdout line.
fn start(data_dir: &std::path::Path) -> (Running, String) {
    let mut child = Command::new(BIN)
        .arg("127.0.0.1:0")
        .arg(data_dir)
        .env("RELAYFOLD_CONTROL_DIALKEY", DK_CONTROL)
        .env("RELAYFOLD_ADMIN_KEYS", key_to_z32(&public_key_of(DK_ADMIN)))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn relayfold-control");
    let stdout = child.stdout.take().unwrap();
    let guard = Running(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the binary must announce its address within 30s");
    let addr = line
        .strip_prefix("relayfold-control listening on ")
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_else(|| panic!("unexpected startup line: {line:?}"))
        .to_string();
    assert!(
        !addr.ends_with(":0"),
        "must report the real port, got {addr}"
    );
    (guard, format!("http://{addr}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_serves_signed_calls_and_refuses_unsigned_ones() {
    let dir = tempdir("binary");
    let (_child, base) = start(dir.path());
    let client = Client::new();
    let a = Signer::from_dialkey(DK_A);

    let unsigned = client
        .post(format!("{base}/register"))
        .body(register_body("bin-1", "http://127.0.0.1:1"))
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status().as_u16(), 401);

    let signed = post_signed(
        &client,
        &base,
        &a,
        "/register",
        &register_body("bin-1", "http://127.0.0.1:1"),
        now_unix(),
    )
    .await;
    assert!(signed.status().is_success(), "got {}", signed.status());

    let list = client
        .get(format!("{base}/instances"))
        .send()
        .await
        .unwrap();
    let text = list.text().await.unwrap();
    assert!(
        text.contains("bin-1") && text.contains(&key_to_z32(&a.public_key())),
        "{text}"
    );
    assert!(
        dir.path().join(relayfold_control::store::LOG_FILE).exists(),
        "the binary must be writing its log under the data dir"
    );
}

#[test]
fn the_binary_refuses_to_start_without_a_control_dialkey() {
    let dir = tempdir("binary-nokey");
    let out = Command::new(BIN)
        .arg("127.0.0.1:0")
        .arg(dir.path())
        .env_remove("RELAYFOLD_CONTROL_DIALKEY")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("RELAYFOLD_CONTROL_DIALKEY"),
        "stderr: {stderr}"
    );
}

#[test]
fn the_binary_refuses_a_malformed_admin_key() {
    let dir = tempdir("binary-badadmin");
    let out = Command::new(BIN)
        .arg("127.0.0.1:0")
        .arg(dir.path())
        .env("RELAYFOLD_CONTROL_DIALKEY", DK_CONTROL)
        .env("RELAYFOLD_ADMIN_KEYS", "not-a-key")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bad admin key"));
}
