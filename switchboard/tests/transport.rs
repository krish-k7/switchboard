//! The HTTPS transport against a local TLS server whose replies each test
//! scripts, counting TCP connections to show reuse, discard, and no retry.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use serde_json::json;
use switchboard::hub_transport::{ClientConfig, HubClient, HubError, MAX_CONNECTIONS};

struct Server {
    port: u16,
    connections: Arc<Mutex<usize>>,
    requests: Arc<Mutex<Vec<String>>>,
    replies: Arc<Mutex<Vec<Vec<u8>>>>,
    gate: Arc<Mutex<Option<Arc<Barrier>>>>,
    root: PathBuf,
}

fn reply(status: &str, content_type: &str, body: &str, extra: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}\r\n{body}", body.len()).into_bytes()
}

fn ok() -> Vec<u8> {
    reply("200 OK", "application/json", r#"{"ok": true}"#, "")
}

impl Server {
    fn start() -> Self {
        let root = std::env::temp_dir().join(format!("sbt-{}", switchboard::identity::uuid4_hex()));
        std::fs::create_dir_all(&root).unwrap();
        let server_key = rcgen::KeyPair::generate().unwrap();
        let server_cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&server_key)
            .unwrap();
        let client_key = rcgen::KeyPair::generate().unwrap();
        let client_cert = rcgen::CertificateParams::new(vec!["client".into()])
            .unwrap()
            .self_signed(&client_key)
            .unwrap();
        std::fs::write(root.join("ca.pem"), server_cert.pem()).unwrap();
        std::fs::write(root.join("cert.pem"), client_cert.pem()).unwrap();
        std::fs::write(root.join("key.pem"), client_key.serialize_pem()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![server_cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::try_from(server_key.serialize_der()).unwrap(),
            )
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server {
            port,
            connections: Arc::new(Mutex::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
            replies: Arc::new(Mutex::new(Vec::new())),
            gate: Arc::new(Mutex::new(None)),
            root,
        };
        let (connections, requests, replies, gate) = (
            server.connections.clone(),
            server.requests.clone(),
            server.replies.clone(),
            server.gate.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                *connections.lock().unwrap() += 1;
                let (config, requests, replies, gate) = (
                    config.clone(),
                    requests.clone(),
                    replies.clone(),
                    gate.clone(),
                );
                std::thread::spawn(move || {
                    let connection = rustls::ServerConnection::new(config).unwrap();
                    let mut tls = BufReader::new(rustls::StreamOwned::new(connection, stream));
                    loop {
                        let mut line = String::new();
                        if tls.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut length = 0;
                        loop {
                            let mut header = String::new();
                            tls.read_line(&mut header).unwrap();
                            if header == "\r\n" {
                                break;
                            }
                            if let Some(value) =
                                header.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = value.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; length];
                        tls.read_exact(&mut body).unwrap();
                        requests.lock().unwrap().push(line.trim().to_string());
                        let waiting = gate.lock().unwrap().clone();
                        if let Some(barrier) = waiting {
                            barrier.wait();
                            barrier.wait();
                        }
                        let next = {
                            let mut queue = replies.lock().unwrap();
                            if queue.is_empty() {
                                ok()
                            } else {
                                queue.remove(0)
                            }
                        };
                        let close = String::from_utf8_lossy(&next).contains("Connection: close")
                            || next.starts_with(b"HTTP/1.0");
                        let stream = tls.get_mut();
                        if stream
                            .write_all(&next)
                            .and_then(|_| stream.flush())
                            .is_err()
                            || close
                        {
                            stream.conn.send_close_notify();
                            let _ = stream.flush();
                            return;
                        }
                    }
                });
            }
        });
        server
    }

    fn client(&self) -> HubClient {
        self.client_with_limit(switchboard::hub_transport::DEFAULT_RESPONSE_LIMIT)
    }

    fn client_with_limit(&self, limit: usize) -> HubClient {
        let path = |name: &str| self.root.join(name).to_string_lossy().into_owned();
        HubClient::with_limit(
            &ClientConfig {
                hub_url: format!("https://localhost:{}", self.port),
                ca: Some(path("ca.pem")),
                cert: path("cert.pem"),
                key: path("key.pem"),
            },
            limit,
        )
        .unwrap()
    }

    fn script(&self, replies: Vec<Vec<u8>>) {
        *self.replies.lock().unwrap() = replies;
    }

    fn connections(&self) -> usize {
        *self.connections.lock().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn json_of(value: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    serde_json::Value::Object(value)
}

#[test]
fn sequential_reuse_and_close() {
    let server = Server::start();
    let client = server.client();
    assert_eq!(
        json_of(client.post("/v1/send", &json!({"text": "one"})).unwrap()),
        json!({"ok": true})
    );
    client.request("GET", "/health", None).unwrap();
    assert_eq!(server.connections(), 1);
    assert_eq!(
        *server.requests.lock().unwrap(),
        ["POST /v1/send HTTP/1.1", "GET /health HTTP/1.1"]
    );
    client.close();
    assert_eq!(
        client.request("GET", "/health", None).unwrap_err(),
        HubError::Os("server client is closed".into())
    );
}

#[test]
fn old_server_404_keeps_status_and_code_and_discards() {
    let server = Server::start();
    let client = server.client();
    client.request("GET", "/v1/peers", None).unwrap();
    server.script(vec![reply(
        "404 Not Found",
        "application/json",
        r#"{"ok":false,"error":"not_found"}"#,
        "",
    )]);
    let error = client.request("GET", "/v1/peers", None).unwrap_err();
    assert_eq!(
        error,
        HubError::Http {
            status: 404,
            code: Some("not_found".into())
        }
    );
    assert_eq!(error.to_string(), "server HTTP 404 (not_found)");
    client.request("GET", "/v1/peers", None).unwrap();
    assert_eq!(server.connections(), 2);
}

#[test]
fn validation_before_connection() {
    let server = Server::start();
    let client = server.client();
    assert_eq!(
        client.request("GET", "/redirect-target", None).unwrap_err(),
        HubError::Value("unsupported server endpoint".into())
    );
    assert_eq!(
        client
            .post("/v1/send", &json!({"text": "a".repeat(65536)}))
            .unwrap_err(),
        HubError::Value("server request too large".into())
    );
    // The removed viewer paths are not server endpoints.
    for path in ["/", "/v1/view"] {
        assert_eq!(
            client.request("GET", path, None).unwrap_err(),
            HubError::Value("unsupported server endpoint".into())
        );
    }
    assert_eq!(server.connections(), 0);
}

#[test]
fn error_responses_discard_without_retry() {
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (
            reply("302 Found", "application/json", "{}", "Location: /x\r\n"),
            "server HTTP 302",
        ),
        (reply("200 OK", "application/json", "not JSON", ""), ""),
        (
            reply("200 OK", "application/json", "[]", ""),
            "server returned an invalid response",
        ),
        (
            reply("200 OK", "application/json", r#"{"ok":false}"#, ""),
            "server returned an invalid response",
        ),
        (
            reply("200 OK", "text/html", r#"{"ok":true}"#, ""),
            "server returned a non-JSON response",
        ),
        (
            reply("200 OK", "application/json", &"x".repeat(65), ""),
            "server response too large",
        ),
    ];
    for (raw, message) in cases {
        let server = Server::start();
        let client = server.client_with_limit(64);
        client.request("GET", "/health", None).unwrap();
        server.script(vec![raw]);
        let error = client
            .post("/v1/send", &json!({"text": "never retry"}))
            .unwrap_err();
        if !message.is_empty() {
            assert_eq!(error.to_string(), message);
        }
        assert_eq!(server.requests.lock().unwrap().len(), 2, "{message}");
        client.request("GET", "/health", None).unwrap();
        assert_eq!(server.connections(), 2, "{message}");
    }
}

#[test]
fn http10_and_connection_close_are_not_reused() {
    let server = Server::start();
    let client = server.client();
    server.script(vec![
        reply("200 OK", "application/json", r#"{"ok": true}"#, "Connection: close\r\n"),
        b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n\r\n{\"ok\": true}".to_vec(),
    ]);
    client.request("GET", "/health", None).unwrap();
    client.request("GET", "/health", None).unwrap();
    client.request("GET", "/health", None).unwrap();
    assert_eq!(server.connections(), 3);
}

#[test]
fn http_error_codes_are_bounded_and_safe() {
    for (body, code) in [
        (
            r#"{"error":"stale_session"}"#.to_string(),
            Some("stale_session"),
        ),
        (r#"{"error":"bad\nforged"}"#.to_string(), None),
        (r#"{"error":123}"#.to_string(), None),
        ("not-json".to_string(), None),
        ("x".repeat(65537), None),
    ] {
        let server = Server::start();
        let client = server.client();
        server.script(vec![reply("409 Conflict", "application/json", &body, "")]);
        let error = client
            .post("/v1/send", &json!({"text": "no retry"}))
            .unwrap_err();
        assert_eq!(
            error,
            HubError::Http {
                status: 409,
                code: code.map(str::to_string)
            }
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}

#[test]
fn thread_lanes_are_bounded() {
    let server = Server::start();
    let client = Arc::new(server.client());
    let barrier = Arc::new(Barrier::new(MAX_CONNECTIONS + 1));
    *server.gate.lock().unwrap() = Some(barrier.clone());
    let workers: Vec<_> = (0..MAX_CONNECTIONS)
        .map(|_| {
            let client = client.clone();
            std::thread::spawn(move || client.request("GET", "/health", None).map(|_| ()))
        })
        .collect();
    barrier.wait(); // Every lane is busy inside a request.
    let error = client.request("GET", "/health", None).unwrap_err();
    assert_eq!(
        error,
        HubError::Os("server connection limit reached".into())
    );
    *server.gate.lock().unwrap() = None;
    barrier.wait();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    // Their threads have ended, so their idle lanes give way to a new one.
    std::thread::sleep(Duration::from_millis(50));
    client.request("GET", "/health", None).unwrap();
    client.close();
}
