//! Local HTTP server that streams the filtered TS of the channel being watched to the webview (mpegts.js).
//!
//! `GET /live.ts` answers with a chunked `video/mp2t` body fed from [`Hub::broadcast`]. Only 127.0.0.1 is bound.
//! Switching channels calls [`Hub::reset`], which ends the current responses so the player reconnects cleanly.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Chunks buffered per client (~4 s at 15 Mbit/s with 64 KiB chunks) before it counts as stalled.
const CLIENT_QUEUE: usize = 128;

#[derive(Default)]
pub struct Hub {
    clients: Mutex<Vec<SyncSender<Arc<Vec<u8>>>>>,
    /// Chunks not delivered because a client's queue was full (the player didn't keep up).
    dropped: AtomicU64,
}

impl Hub {
    /// Starts the server on an ephemeral 127.0.0.1 port and returns it.
    pub fn start(self: &Arc<Self>) -> std::io::Result<u16> {
        let l = TcpListener::bind(("127.0.0.1", 0))?;
        let port = l.local_addr()?.port();
        let hub = self.clone();
        std::thread::Builder::new()
            .name("ts-http".into())
            .spawn(move || {
                for conn in l.incoming().flatten() {
                    let hub = hub.clone();
                    let _ = std::thread::Builder::new()
                        .name("ts-client".into())
                        .spawn(move || {
                            let _ = serve(&hub, conn);
                        });
                }
            })?;
        Ok(port)
    }

    pub fn broadcast(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let data = Arc::new(data);
        self.clients
            .lock()
            .unwrap()
            .retain(|c| match c.try_send(data.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    // slow client: drop this chunk, keep the client
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Ends every open response.
    pub fn reset(&self) {
        self.clients.lock().unwrap().clear();
    }

    fn subscribe(&self) -> Receiver<Arc<Vec<u8>>> {
        let (tx, rx) = sync_channel(CLIENT_QUEUE);
        self.clients.lock().unwrap().push(tx);
        rx
    }
}

const CORS: &str = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\n";

fn serve(hub: &Hub, mut conn: TcpStream) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(conn.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let mut parts = request.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method == "OPTIONS" {
        return conn.write_all(
            format!(
                "HTTP/1.1 204 No Content\r\n{CORS}Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        );
    }
    if method != "GET" || !path.starts_with("/live.ts") {
        return conn.write_all(
            format!(
                "HTTP/1.1 404 Not Found\r\n{CORS}Content-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        );
    }
    let rx = hub.subscribe();
    conn.set_nodelay(true)?;
    conn.write_all(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-store\r\n{CORS}Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )?;
    while let Ok(chunk) = rx.recv() {
        conn.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())?;
        conn.write_all(&chunk)?;
        conn.write_all(b"\r\n")?;
    }
    conn.write_all(b"0\r\n\r\n")
}
