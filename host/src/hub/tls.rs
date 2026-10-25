//! TLS-слушатель LAN-хаба (D47-3): rustls поверх TCP как `axum::serve::Listener`.
//!
//! Принимающий цикл не ждёт рукопожатия: `accept` TCP -> рукопожатие в
//! отдельной задаче (таймаут 10 с) -> готовый поток в канал -> `Listener::accept`
//! отдаёт из канала. Браузер, не принявший самоподписанный сертификат, рвёт
//! рукопожатия постоянно, поэтому их ошибки -- только `debug!`.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько готовых потоков ждут `axum::serve`; приём дальше упирается в TCP backlog.
const READY_QUEUE: usize = 64;

/// rustls-конфигурация хаба: провайдер ring, ALPN только `http/1.1`
/// (WebSocket идёт по HTTP/1.1).
pub fn server_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<Arc<ServerConfig>> {
    let key = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_der));
    let mut config = ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .context("rustls: версии протокола")?
    .with_no_client_auth()
    .with_single_cert(vec![CertificateDer::from(cert_der)], key)
    .context("rustls: сертификат хаба")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Слушатель для `axum::serve`: отдаёт потоки с завершённым TLS-рукопожатием.
pub struct TlsListener {
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local_addr: SocketAddr,
    accept_task: JoinHandle<()>,
}

impl TlsListener {
    /// Занимает порт и запускает принимающую задачу.
    pub async fn bind(addr: SocketAddr, config: Arc<ServerConfig>) -> Result<Self> {
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("не удалось занять {addr}"))?;
        let local_addr = tcp.local_addr()?;
        let (tx, ready) = mpsc::channel(READY_QUEUE);
        let acceptor = TlsAcceptor::from(config);
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match tcp.accept().await {
                    Ok(pair) => pair,
                    Err(err) => {
                        // EMFILE и подобное: не крутим цикл вхолостую.
                        tracing::warn!("hub: accept failed: {err}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let _ = stream.set_nodelay(true);
                let acceptor = acceptor.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(tls)) => {
                            let _ = tx.send((tls, peer)).await;
                        }
                        Ok(Err(err)) => tracing::debug!("hub: tls handshake with {peer}: {err}"),
                        Err(_) => tracing::debug!("hub: tls handshake with {peer} timed out"),
                    }
                });
            }
        });
        Ok(Self {
            ready,
            local_addr,
            accept_task,
        })
    }
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(pair) => pair,
            // Принимающая задача жива, пока жив слушатель (она держит `tx`).
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}
