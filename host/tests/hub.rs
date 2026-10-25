//! Интеграционный тест LAN-хаба (D47-3): HTTPS, WebSocket по TLS и локальный
//! WebSocket без TLS на портах 0 во временном каталоге.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt as _, StreamExt as _};
use proto::signal::{Role, SignalMessage};
use rcdesk_host::hub::{self, cert, web, HubConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, connect_async_tls_with_config, Connector};

/// Только для теста: принимает любой сертификат и запоминает предъявленный.
#[derive(Debug)]
struct AcceptAny {
    seen: Mutex<Option<Vec<u8>>>,
}

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        *self.seen.lock().expect("seen") = Some(end_entity.as_ref().to_vec());
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tokio_rustls::rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_config(verifier: Arc<AcceptAny>) -> Arc<ClientConfig> {
    let config = ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("versions")
    .dangerous()
    .with_custom_certificate_verifier(verifier)
    .with_no_client_auth();
    Arc::new(config)
}

async fn authenticate<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> SignalMessage
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let hello = SignalMessage::Hello {
        role: Role::Client,
        version: "test".to_string(),
    };
    ws.send(Message::text(serde_json::to_string(&hello).expect("json")))
        .await
        .expect("send hello");
    let auth = SignalMessage::ClientAuth { token: None };
    ws.send(Message::text(serde_json::to_string(&auth).expect("json")))
        .await
        .expect("send auth");
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .expect("ответ в срок")
            .expect("поток открыт")
            .expect("кадр");
        if let Message::Text(text) = msg {
            let parsed: SignalMessage = serde_json::from_str(text.as_str()).expect("signal json");
            if matches!(parsed, SignalMessage::Authenticated { .. }) {
                return parsed;
            }
        }
    }
}

#[tokio::test]
async fn hub_serves_https_and_signaling_over_tls_and_locally() {
    let dir = std::env::temp_dir().join(format!("rcdesk-hub-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let hub = hub::start(HubConfig {
        data_dir: dir.clone(),
        port: 0,
        local_port: 0,
    })
    .await
    .expect("start hub");

    let verifier = Arc::new(AcceptAny {
        seen: Mutex::new(None),
    });
    let config = client_config(verifier.clone());

    // HTTP GET / поверх TLS.
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", hub.tls_addr.port()))
        .await
        .expect("tcp");
    let connector = tokio_rustls::TlsConnector::from(config.clone());
    let mut tls = connector
        .connect(ServerName::try_from("localhost").expect("name"), tcp)
        .await
        .expect("tls handshake");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut raw = Vec::new();
    let _ = tls.read_to_end(&mut raw).await;
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    let expected_status = if web::embedded_files().is_empty() {
        "503"
    } else {
        "200"
    };
    assert!(
        text.starts_with(&format!("http/1.1 {expected_status}")),
        "ответ: {text}"
    );
    assert!(text.contains("content-security-policy:"), "ответ: {text}");
    assert!(text.contains("connect-src 'self';"), "ответ: {text}");

    // Отпечаток хаба совпадает с SPKI предъявленного сертификата.
    let presented = verifier
        .seen
        .lock()
        .expect("seen")
        .clone()
        .expect("сертификат предъявлен");
    assert_eq!(
        cert::cert_spki_sha256_hex(&presented).as_deref(),
        Some(hub.fingerprint.as_str())
    );

    // WebSocket по TLS.
    let url = format!("wss://127.0.0.1:{}/ws", hub.tls_addr.port());
    let (mut ws, _) =
        connect_async_tls_with_config(url, None, false, Some(Connector::Rustls(config.clone())))
            .await
            .expect("wss connect");
    match authenticate(&mut ws).await {
        SignalMessage::Authenticated { lan_hub, .. } => assert!(lan_hub),
        other => panic!("ожидался Authenticated: {other:?}"),
    }

    // Локальный WebSocket без TLS и /healthz.
    let url = format!("ws://127.0.0.1:{}/ws", hub.local_addr.port());
    let (mut ws, _) = connect_async(url).await.expect("ws connect");
    match authenticate(&mut ws).await {
        SignalMessage::Authenticated { lan_hub, .. } => assert!(lan_hub),
        other => panic!("ожидался Authenticated: {other:?}"),
    }
    let mut plain = tokio::net::TcpStream::connect(hub.local_addr)
        .await
        .expect("tcp local");
    plain
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut raw = Vec::new();
    let _ = plain.read_to_end(&mut raw).await;
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200"));

    hub.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn host_pins_the_hub_key_on_first_connect_and_rejects_another() {
    use rcdesk_host::signaling::{PinStore, SignalingClient};

    let base = std::env::temp_dir().join(format!("rcdesk-hub-pin-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let hub_dir = base.join("hub");
    let host_dir = base.join("host");
    let hub = hub::start(HubConfig {
        data_dir: hub_dir,
        port: 0,
        local_port: 0,
    })
    .await
    .expect("start hub");

    let url = format!("wss://127.0.0.1:{}/ws", hub.tls_addr.port());
    let store = PinStore::new(&host_dir, &url, 0o600).expect("pin store");
    let connect = || SignalingClient::connect(&url, "pin-test", None, None, Some(store.clone()));

    // Первое знакомство: подключились, ключ закреплён.
    connect().await.map(|_| ()).expect("первое подключение");
    let saved = std::fs::read_to_string(store.path()).expect("файл закрепления");
    assert!(
        store
            .path()
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("hub-pin-")),
        "{:?}",
        store.path()
    );
    assert_eq!(saved, format!("{}\n", hub.fingerprint));

    // Второе подключение с тем же ключом проходит.
    connect().await.map(|_| ()).expect("второе подключение");

    // Чужой закреплённый ключ: отказ, файл не перезаписан.
    let foreign = format!("{}\n", "0".repeat(64));
    std::fs::write(store.path(), &foreign).expect("write");
    let err = match connect().await {
        Ok(_) => panic!("подключение с чужим ключом должно провалиться"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains(&hub.fingerprint), "{err}");
    assert!(err.contains("hub-pin-"), "{err}");
    assert_eq!(std::fs::read_to_string(store.path()).unwrap(), foreign);

    hub.stop();
    let _ = std::fs::remove_dir_all(&base);
}
