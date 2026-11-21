use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

fn eyra_bin() -> &'static str {
    env!("CARGO_BIN_EXE_eyra")
}

struct BridgeProc(Child);
impl Drop for BridgeProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Bind ephemeral port");
    listener.local_addr().unwrap().port()
}

fn http_call(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    origin: Option<&str>,
    body: Option<&str>,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).expect("Connect bridge");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let payload = body.unwrap_or("");
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        method, path, port
    );
    if let Some(tok) = token {
        req.push_str(&format!("Authorization: Bearer {}\r\n", tok));
    }
    if let Some(orig) = origin {
        req.push_str(&format!("Origin: {}\r\n", orig));
    }
    if !payload.is_empty() {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    req.push_str("\r\n");
    req.push_str(payload);

    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();

    let (head, body_part) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, head.to_string(), body_part.to_string())
}

#[test]
fn bridge_enforces_auth_cors_read_only_and_path_safety() {
    let port = pick_free_port();
    let token = "test_secret_token_1234567890abcdef";
    let log_path =
        std::env::temp_dir().join(format!("eyra_bridge_audit_{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let child = Command::new(eyra_bin())
        .args([
            "--read-only",
            "--log-file",
            log_path.to_str().unwrap(),
            "serve",
            "--port",
            &port.to_string(),
            "--token",
            token,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn eyra serve");
    let _guard = BridgeProc(child);

    // Wait for bridge to bind
    let mut ready = false;
    for _ in 0..30 {
        if TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(ready, "eyra serve did not bind on 127.0.0.1:{}", port);

    // 1. Public /api/health succeeds and returns allowed CORS header
    let (status, head, body) = http_call(
        port,
        "GET",
        "/api/health",
        None,
        Some("http://localhost:5173"),
        None,
    );
    assert_eq!(status, 200);
    assert!(head.contains("Access-Control-Allow-Origin: http://localhost:5173"));
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["name"], "eyra");
    assert_eq!(v["read_only"], true);

    // 2. Unauthorized origin gets no CORS header
    let (_, evil_head, _) = http_call(
        port,
        "GET",
        "/api/health",
        None,
        Some("https://evil.example.com"),
        None,
    );
    assert!(!evil_head.contains("Access-Control-Allow-Origin"));

    // 3. Protected routes reject missing/wrong token with 401
    let (unauth_status, _, _) = http_call(port, "GET", "/api/windows", None, None, None);
    assert_eq!(unauth_status, 401);

    let (wrong_status, _, _) =
        http_call(port, "GET", "/api/windows", Some("wrong_token"), None, None);
    assert_eq!(wrong_status, 401);

    // 4. Authorized /api/windows succeeds and writes audit log
    let (win_status, _, win_body) = http_call(port, "GET", "/api/windows", Some(token), None, None);
    assert_eq!(win_status, 200);
    let win_v: Value = serde_json::from_str(&win_body).unwrap();
    assert!(win_v["windows"].is_array());

    // 5. Read-only mode blocks /api/action/click with 403
    let (ro_status, _, _) = http_call(
        port,
        "POST",
        "/api/action/click",
        Some(token),
        None,
        Some(r#"{"x":100,"y":100}"#),
    );
    assert_eq!(ro_status, 403);

    // 6. Path traversal attempt on media route returns 400
    let (trav_status, _, _) = http_call(
        port,
        "GET",
        &format!("/api/watch/media/..%2Fsecret.jpg?token={}", token),
        None,
        None,
        None,
    );
    assert_eq!(trav_status, 400);

    let (trav_status2, _, _) = http_call(
        port,
        "GET",
        &format!("/api/watch/media/../secret.jpg?token={}", token),
        None,
        None,
        None,
    );
    assert_eq!(trav_status2, 400);

    // 7. /api/logs returns recorded HTTP calls
    let (log_status, _, log_body) =
        http_call(port, "GET", "/api/logs?tail=10", Some(token), None, None);
    assert_eq!(log_status, 200);
    let log_v: Value = serde_json::from_str(&log_body).unwrap();
    assert!(!log_v["entries"].as_array().unwrap().is_empty());

    drop(_guard);
    let _ = std::fs::remove_file(&log_path);
}
