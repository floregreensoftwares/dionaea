#![allow(clippy::unwrap_used)] // test helpers build certificates and sockets that cannot fail
//! The store's connections, tested against servers started in the test: that
//! connections are reused and recovered when the server closes them, and that
//! TLS trusts only what it is told to, checks the name and fails cleanly.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use dionaea_store::{Config, Error, Store, TlsConfig};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Behaviour {
    /// Answer with a length and keep the connection open.
    KeepAlive,
    /// Answer with a length, claim keep-alive, then close the socket.
    CloseQuietly,
    /// Answer with `Connection: close`.
    SayClose,
}

struct Fake {
    port: u16,
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

/// Reads one request (head and body) and answers it. False at the end of the
/// connection.
fn serve_request(stream: &mut (impl Read + Write), behaviour: Behaviour) -> bool {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    while !buffer.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => buffer.push(byte[0]),
            _ => return false,
        }
    }
    let head = String::from_utf8_lossy(&buffer).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if stream.read_exact(&mut body).is_err() {
        return false;
    }
    let connection = if behaviour == Behaviour::SayClose {
        "Connection: close\r\n"
    } else {
        ""
    };
    let answer = format!("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n{connection}\r\n1\n");
    stream.write_all(answer.as_bytes()).is_ok() && stream.flush().is_ok()
}

fn serve_connection(mut stream: impl Read + Write, behaviour: Behaviour, requests: &AtomicUsize) {
    while serve_request(&mut stream, behaviour) {
        requests.fetch_add(1, Ordering::Relaxed);
        if behaviour != Behaviour::KeepAlive {
            return;
        }
    }
}

fn start(behaviour: Behaviour, tls: Option<Arc<ServerConfig>>) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let (c, r) = (Arc::clone(&connections), Arc::clone(&requests));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            c.fetch_add(1, Ordering::Relaxed);
            let r = Arc::clone(&r);
            let tls = tls.clone();
            thread::spawn(move || {
                stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                match tls {
                    None => serve_connection(stream, behaviour, &r),
                    Some(config) => {
                        let Ok(connection) = ServerConnection::new(config) else {
                            return;
                        };
                        serve_connection(StreamOwned::new(connection, stream), behaviour, &r);
                    }
                }
            });
        }
    });
    Fake {
        port,
        connections,
        requests,
    }
}

fn store(port: u16, tls: Option<TlsConfig>) -> Result<Store, Error> {
    store_with_timeout(port, tls, Duration::from_secs(5))
}

fn store_with_timeout(
    port: u16,
    tls: Option<TlsConfig>,
    timeout: Duration,
) -> Result<Store, Error> {
    let mut config = Config::new("127.0.0.1", port, "dionaea");
    config.timeout = timeout;
    config.tls = tls;
    Store::new(config)
}

// Keep-alive

#[test]
fn connections_are_reused_across_calls() {
    let fake = start(Behaviour::KeepAlive, None);
    let store = store(fake.port, None).unwrap();
    for _ in 0..10 {
        store.ping().unwrap();
    }
    assert_eq!(store.connections_opened(), 1);
    assert_eq!(fake.connections.load(Ordering::Relaxed), 1);
    wait_for(&fake.requests, 10);
}

#[test]
fn clones_share_the_idle_connections() {
    let fake = start(Behaviour::KeepAlive, None);
    let first = store(fake.port, None).unwrap();
    let second = first.clone();
    first.ping().unwrap();
    second.ping().unwrap();
    first.ping().unwrap();
    assert_eq!(first.connections_opened(), 1);
}

#[test]
fn a_connection_the_server_closed_is_replaced_without_failing_the_call() {
    let fake = start(Behaviour::CloseQuietly, None);
    let store = store(fake.port, None).unwrap();
    for _ in 0..5 {
        store
            .ping()
            .expect("a stale connection must not fail the call");
    }
    // The server closes after each answer, so each call finds a dead idle
    // connection and opens a new one.
    wait_for(&fake.requests, 5);
    assert!(store.connections_opened() >= 5);
}

#[test]
fn a_server_that_says_close_gets_a_new_connection_each_time() {
    let fake = start(Behaviour::SayClose, None);
    let store = store(fake.port, None).unwrap();
    for _ in 0..4 {
        store.ping().unwrap();
    }
    assert_eq!(store.connections_opened(), 4);
}

#[test]
fn concurrent_calls_all_succeed_and_connections_stay_bounded() {
    let fake = start(Behaviour::KeepAlive, None);
    let store = store(fake.port, None).unwrap();
    let done = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (store, done) = (store.clone(), Arc::clone(&done));
            thread::spawn(move || {
                for _ in 0..50 {
                    store.ping().unwrap();
                    done.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(done.load(Ordering::Relaxed), 400);
    assert!(store.connections_opened() <= 400);
    assert!(
        store.connections_opened() < 100,
        "{} connections for 400 calls",
        store.connections_opened()
    );
}

// TLS

struct Pki {
    ca_pem: String,
    server_config: Arc<ServerConfig>,
}

/// A CA and a server certificate for `names`, signed by it.
fn pki(names: &[&str]) -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.clone().self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, &ca_key);

    let server_key = KeyPair::generate().unwrap();
    let server_params =
        CertificateParams::new(names.iter().map(ToString::to_string).collect::<Vec<_>>()).unwrap();
    let server = server_params.signed_by(&server_key, &issuer).unwrap();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![server.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
        )
        .unwrap();
    Pki {
        ca_pem: ca.pem(),
        server_config: Arc::new(server_config),
    }
}

fn write_pem(pem: &str, name: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("dionaea-store-{}-{name}.pem", std::process::id()));
    std::fs::write(&path, pem).unwrap();
    path
}

fn tls_to(ca: &Path) -> Option<TlsConfig> {
    Some(TlsConfig {
        ca_file: Some(ca.to_path_buf()),
        // The test servers listen on 127.0.0.1; their certificates name `localhost`.
        server_name: Some("localhost".to_owned()),
    })
}

#[test]
fn tls_to_a_server_with_a_trusted_certificate_works_and_reuses_the_connection() {
    let pki = pki(&["localhost"]);
    let ca = write_pem(&pki.ca_pem, "trusted");
    let fake = start(Behaviour::KeepAlive, Some(pki.server_config));
    let store = store(fake.port, tls_to(&ca)).unwrap();
    for _ in 0..5 {
        store.ping().unwrap();
    }
    assert_eq!(
        store.connections_opened(),
        1,
        "one TLS handshake serves every call"
    );
    wait_for(&fake.requests, 5);
}

#[test]
fn a_certificate_from_an_unknown_authority_is_refused() {
    let server = pki(&["localhost"]);
    let other = pki(&["localhost"]);
    let wrong_ca = write_pem(&other.ca_pem, "wrong-ca");
    let fake = start(Behaviour::KeepAlive, Some(server.server_config));
    let error = store(fake.port, tls_to(&wrong_ca))
        .unwrap()
        .ping()
        .unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
    assert_eq!(
        fake.requests.load(Ordering::Relaxed),
        0,
        "no request went out over an untrusted connection"
    );
}

#[test]
fn a_certificate_for_another_name_is_refused() {
    let pki = pki(&["something-else.example"]);
    let ca = write_pem(&pki.ca_pem, "wrong-name");
    let fake = start(Behaviour::KeepAlive, Some(pki.server_config));
    let error = store(fake.port, tls_to(&ca)).unwrap().ping().unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
    wait_for(&fake.requests, 0);
}

#[test]
fn the_server_name_can_be_set_apart_from_the_host() {
    let pki = pki(&["clickhouse.internal"]);
    let ca = write_pem(&pki.ca_pem, "named");
    let fake = start(Behaviour::KeepAlive, Some(pki.server_config));
    let tls = Some(TlsConfig {
        ca_file: Some(ca),
        server_name: Some("clickhouse.internal".to_owned()),
    });
    store(fake.port, tls).unwrap().ping().unwrap();
    wait_for(&fake.requests, 1);
}

#[test]
fn without_a_ca_file_only_public_roots_are_trusted() {
    let pki = pki(&["localhost"]);
    let fake = start(Behaviour::KeepAlive, Some(pki.server_config));
    let tls = Some(TlsConfig::default());
    let error = store(fake.port, tls).unwrap().ping().unwrap_err();
    assert!(matches!(error, Error::Io(_)), "{error:?}");
}

#[test]
fn tls_against_a_plain_server_and_plain_against_a_tls_server_fail_without_hanging() {
    let pki = pki(&["localhost"]);
    let ca = write_pem(&pki.ca_pem, "mismatch");
    let plain = start(Behaviour::KeepAlive, None);
    let started = std::time::Instant::now();
    let quick = Duration::from_secs(1);
    assert!(
        store_with_timeout(plain.port, tls_to(&ca), quick)
            .unwrap()
            .ping()
            .is_err()
    );
    let tls_server = start(Behaviour::KeepAlive, Some(pki.server_config));
    assert!(
        store_with_timeout(tls_server.port, None, quick)
            .unwrap()
            .ping()
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(15));
}

#[test]
fn bad_tls_settings_are_reported_when_the_store_is_created() {
    let missing = PathBuf::from("this-file-does-not-exist.pem");
    assert!(matches!(store(1, tls_to(&missing)), Err(Error::Tls(_))));
    let garbage = write_pem("not a certificate", "garbage");
    assert!(matches!(store(1, tls_to(&garbage)), Err(Error::Tls(_))));
    let empty = write_pem("", "empty");
    assert!(matches!(store(1, tls_to(&empty)), Err(Error::Tls(_))));
    let bad_name = Some(TlsConfig {
        ca_file: None,
        server_name: Some("not a name!".to_owned()),
    });
    assert!(matches!(store(1, bad_name), Err(Error::Tls(_))));
}

/// Waits for a counter the fake server updates after it answers, which can
/// happen just after the client has its answer.
fn wait_for(counter: &AtomicUsize, expected: usize) {
    for _ in 0..200 {
        if counter.load(Ordering::Relaxed) == expected {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(counter.load(Ordering::Relaxed), expected);
}
