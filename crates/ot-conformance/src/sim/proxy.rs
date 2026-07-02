//! TCP transport proxy between the connector under test and a protocol simulator.
//!
//! The connector talks to the proxy's public port; the proxy forwards byte-for-byte to the
//! simulator. Because the proxy owns every client connection, it can simulate a *transport*
//! failure — close the listener AND kill live sessions — which a simulator built on
//! `tokio-modbus`'s server cannot do (its per-connection tasks are unreachable once spawned).
//! Bringing the transport back re-binds the same public port, so the connector's
//! reconnect-with-backoff finds the device at the address it was configured with.

use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tracing::debug;

#[derive(Default)]
struct Inner {
    accept: Option<tokio::task::JoinHandle<()>>,
    conns: Vec<tokio::task::AbortHandle>,
    up: bool,
}

pub struct TransportProxy {
    public_port: u16,
    target_port: u16,
    inner: Arc<Mutex<Inner>>,
}

impl Drop for TransportProxy {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(handle) = inner.accept.take() {
            handle.abort();
        }
        for conn in inner.conns.drain(..) {
            conn.abort();
        }
    }
}

impl TransportProxy {
    /// Start forwarding a fresh loopback port to `target_port`.
    pub async fn start(target_port: u16) -> Result<TransportProxy, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("proxy bind failed: {e}"))?;
        let public_port = listener
            .local_addr()
            .map_err(|e| format!("proxy local_addr: {e}"))?
            .port();
        let proxy = TransportProxy {
            public_port,
            target_port,
            inner: Arc::new(Mutex::new(Inner::default())),
        };
        proxy.spawn_accept(listener);
        Ok(proxy)
    }

    pub fn port(&self) -> u16 {
        self.public_port
    }

    fn spawn_accept(&self, listener: TcpListener) {
        let inner = self.inner.clone();
        let target = self.target_port;
        let accept_inner = inner.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    break;
                };
                let conn = tokio::spawn(async move {
                    match TcpStream::connect(("127.0.0.1", target)).await {
                        Ok(mut upstream) => {
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                        }
                        Err(e) => debug!("proxy upstream connect failed: {e}"),
                    }
                });
                let mut inner = accept_inner.lock().unwrap();
                inner.conns.retain(|c| !c.is_finished());
                inner.conns.push(conn.abort_handle());
            }
        });
        let mut inner = inner.lock().unwrap();
        inner.accept = Some(handle);
        inner.up = true;
    }

    /// Bring the transport up or down. Down closes the listener and aborts every live
    /// session (the peer sees the TCP connection die); up re-binds the same public port.
    pub async fn set_up(&self, up: bool) -> Result<(), String> {
        if up {
            if self.inner.lock().unwrap().up {
                return Ok(());
            }
            let listener = TcpListener::bind(("127.0.0.1", self.public_port))
                .await
                .map_err(|e| format!("proxy re-bind on port {}: {e}", self.public_port))?;
            self.spawn_accept(listener);
        } else {
            let mut inner = self.inner.lock().unwrap();
            if let Some(handle) = inner.accept.take() {
                handle.abort(); // drops the listener, freeing the port
            }
            for conn in inner.conns.drain(..) {
                conn.abort(); // drops both stream halves: the peer sees a dead session
            }
            inner.up = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// An echo server, a proxied client; drop the transport mid-session and bring it back.
    #[tokio::test]
    async fn forwards_kills_and_restores() {
        let echo = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = echo.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let proxy = TransportProxy::start(echo_port).await.unwrap();

        // forwards
        let mut client = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        // down: the live session dies and new connections are refused
        proxy.set_up(false).await.unwrap();
        let mut dead = [0u8; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut dead))
            .await
            .expect("read returns after transport drop");
        assert!(matches!(read, Ok(0) | Err(_)), "session must be dead: {read:?}");
        assert!(
            TcpStream::connect(("127.0.0.1", proxy.port())).await.is_err(),
            "connect must be refused while down"
        );

        // up again on the same port: a new session works
        proxy.set_up(true).await.unwrap();
        let mut client2 = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        client2.write_all(b"back").await.unwrap();
        let mut buf2 = [0u8; 4];
        client2.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"back");
    }
}
