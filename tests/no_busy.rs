//! Régression d'exigence : la saturation du pool ne doit JAMAIS produire un 503 « busy ».
//!
//! Historiquement `serve()` appelait `write_503()`, qui répondait
//! `503 Service Unavailable` avec le corps `busy`, dès que la file d'accept
//! était pleine (`vite-rs/src/http.rs`). L'exigence est qu'un client reçoive
//! toujours une réponse servie : la saturation retarde, elle ne refuse pas.
//!
//! Le test force un pool minuscule (2 workers) avec bien plus de clients que
//! workers + file, ce qui déclenchait l'ancien chemin de refus.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use vite_rs::headers::DEFAULT_SECURITY_HEADERS;
use vite_rs::http::{self, Outgoing};

/// Pool forcé : 4 workers (plancher du serveur) + file de 32 → 36 places,
/// pour 64 clients. L'ancien chemin `write_503` refusait les 28 restants.
const WORKERS: usize = 4;
const SLOW_MS: u64 = 250;
const CLIENTS: usize = 64;

fn test_port() -> u16 {
    30000 + (std::process::id() as u16 % 12000)
}

fn wait_ready(addr: &str) {
    for _ in 0..60 {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("serveur injoignable sur {addr}");
}

#[test]
fn saturation_never_answers_busy() {
    std::env::set_var("VITE_RS_HTTP_WORKERS", WORKERS.to_string());
    assert_eq!(
        http::worker_count(),
        WORKERS,
        "VITE_RS_HTTP_WORKERS doit piloter la taille du pool"
    );
    assert!(http::queue_cap(WORKERS) < CLIENTS, "le test doit saturer la file");

    let addr = format!("127.0.0.1:{}", test_port());
    let listen = addr.clone();
    std::thread::spawn(move || {
        http::serve(
            &listen,
            |_req: http::Request| -> Outgoing {
                std::thread::sleep(Duration::from_millis(SLOW_MS));
                Outgoing::bytes(200, "text/plain; charset=utf-8", b"ok".to_vec())
            },
            DEFAULT_SECURITY_HEADERS,
        )
        .expect("serve");
    });
    wait_ready(&addr);

    let rejected_before = http::REJECTED.load(Ordering::Relaxed);
    let clients: Vec<_> = (0..CLIENTS)
        .map(|_| {
            let a = addr.clone();
            std::thread::spawn(move || {
                let mut s = TcpStream::connect(&a).expect("connect");
                s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
                s.write_all(b"GET /slow HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
                    .expect("write");
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf);
                let text = String::from_utf8_lossy(&buf).into_owned();
                let status: u16 = text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                (status, text)
            })
        })
        .collect();

    let results: Vec<(u16, String)> = clients
        .into_iter()
        .map(|h| h.join().expect("join client"))
        .collect();

    let busy: Vec<&String> = results
        .iter()
        .filter(|(_, t)| t.contains("busy") || t.contains("503"))
        .map(|(_, t)| t)
        .collect();
    let served = results.iter().filter(|(s, _)| *s == 200).count();

    assert!(
        busy.is_empty(),
        "le serveur a refusé {} client(s) : {:?}",
        busy.len(),
        busy.first()
    );
    assert_eq!(served, CLIENTS, "{served}/{CLIENTS} clients servis en 200");
    assert_eq!(
        http::REJECTED.load(Ordering::Relaxed),
        rejected_before,
        "le compteur de refus doit rester figé : plus aucun chemin de 503"
    );
    assert!(
        http::BACKPRESSURE.load(Ordering::Relaxed) > 0,
        "la contre-pression doit s'être déclenchée (sinon le test ne sature rien)"
    );
}
