//! 機材キー交換（prepare → commit）の往復。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use gpupod_agent::rotate_credentials;
use serde_json::{json, Value};

#[derive(Default)]
struct Seen {
    requests: Vec<SeenRequest>,
    prepared: bool,
}

#[derive(Clone)]
struct SeenRequest {
    path: String,
    authorization: String,
    body: String,
}

/// 中央サーバーの代役。commit は準備前なら 403 を返す。
fn spawn_central(seen: Arc<Mutex<Seen>>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for _ in 0..3 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let _ = handle(stream, seen.clone());
        }
    });
    address
}

fn handle(mut stream: TcpStream, seen: Arc<Mutex<Seen>>) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    let request_line = head.lines().next().unwrap_or_default().to_owned();
    let path = request_line.split_whitespace().nth(1).unwrap_or_default().to_owned();
    let authorization = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.eq_ignore_ascii_case("authorization") {
                Some(value.trim().to_owned())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&body).into_owned();

    let (status, payload) = {
        let mut state = seen.lock().expect("state");
        state.requests.push(SeenRequest {
            path: path.clone(),
            authorization: authorization.clone(),
            body: body.clone(),
        });
        if path.ends_with("/prepare/") {
            state.prepared = true;
            ("200 OK", "{}".to_owned())
        } else if state.prepared {
            let id = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| value.get("id").and_then(|id| id.as_str()).map(str::to_owned))
                .unwrap_or_default();
            ("200 OK", format!("{{\"id\": \"{id}\"}}"))
        } else {
            ("403 Forbidden", "{\"detail\": \"not prepared\"}".to_owned())
        }
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn write_config(directory: &Path, address: std::net::SocketAddr) -> std::path::PathBuf {
    let path = directory.join("node.secret.json");
    let config = json!({
        "url": format!("http://{address}"),
        "node_id": "node-1",
        "token": "old-token",
    });
    std::fs::write(&path, serde_json::to_vec(&config).expect("json")).expect("write");
    path
}

#[test]
fn prepared_pending_is_committed_and_stored() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let address = spawn_central(seen.clone());
    let config_path = write_config(temporary.path(), address);
    let pending_path =
        std::path::PathBuf::from(format!("{}.rotation.secret.json", config_path.display()));
    std::fs::write(
        &pending_path,
        serde_json::to_vec(&json!({ "id": "rotation-1", "token": "candidate-token" }))
            .expect("json"),
    )
    .expect("pending");

    let result = rotate_credentials(&config_path).expect("rotation");
    assert_eq!(result["rotation_id"], "rotation-1");
    assert_eq!(result["status"], "complete");

    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read")).expect("parse");
    assert_eq!(config["token"], "candidate-token");
    assert!(!pending_path.exists());

    let seen = seen.lock().expect("seen");
    let paths: Vec<&str> = seen.requests.iter().map(|item| item.path.as_str()).collect();
    assert_eq!(paths.len(), 3);
    assert!(paths[0].ends_with("/commit/"));
    assert!(paths[1].ends_with("/prepare/"));
    assert!(paths[2].ends_with("/commit/"));
    // commit は候補トークン、prepare は従前のトークンで通す。
    assert_eq!(seen.requests[0].authorization, "Bearer candidate-token");
    assert_eq!(seen.requests[1].authorization, "Bearer old-token");
    assert_eq!(seen.requests[2].authorization, "Bearer candidate-token");
    let prepared: Value =
        serde_json::from_str(&seen.requests[1].body).expect("prepare body");
    assert_eq!(prepared["id"], "rotation-1");
    assert_eq!(prepared["token"], "candidate-token");
}

#[test]
fn fresh_pending_is_created_when_absent() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let address = spawn_central(seen.clone());
    let config_path = write_config(temporary.path(), address);
    let pending_path =
        std::path::PathBuf::from(format!("{}.rotation.secret.json", config_path.display()));
    assert!(!pending_path.exists());

    let result = rotate_credentials(&config_path).expect("rotation");
    assert_eq!(result["status"], "complete");

    let seen = seen.lock().expect("seen");
    // 候補トークンは中央へ先に送られ、そのまま設定へ入る。
    let prepared: Value = serde_json::from_str(&seen.requests[1].body).expect("prepare body");
    let candidate = prepared["token"].as_str().expect("token").to_owned();
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read")).expect("parse");
    assert_eq!(config["token"], candidate);
    assert!(candidate.len() >= 60);
    assert!(!pending_path.exists());
}
