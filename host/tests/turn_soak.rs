//! TURN relay soak (D75): two plain webrtc-rs peers on this machine, one of
//! them relay-only through a real TURN server, kept up past the 600 s marks
//! (channel binding lifetime, coturn nonce lifetime) that today's relay
//! sessions die at. Skipped unless `RCDESK_TURN_SOAK_URL` is set -- it needs a
//! running coturn and takes minutes:
//!
//! ```text
//! RCDESK_TURN_SOAK_URL=turn:127.0.0.1:3478?transport=udp \
//! RCDESK_TURN_SOAK_USER=soak RCDESK_TURN_SOAK_PASS=soak \
//! RCDESK_TURN_SOAK_PEER_IP=<LAN IP the TURN server can reach> \
//! RCDESK_TURN_SOAK_SECS=700 RUST_LOG=info,rtc_turn=debug \
//!   cargo test -p rcdesk-host --test turn_soak -- --nocapture
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use rtc::peer_connection::configuration::RTCIceTransportPolicy;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription,
};
use webrtc::runtime::default_runtime;

struct Handler {
    name: &'static str,
    started: Instant,
    ice_tx: mpsc::UnboundedSender<RTCIceCandidateInit>,
    state_tx: mpsc::UnboundedSender<(&'static str, RTCPeerConnectionState)>,
    dc_tx: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json() {
            let _ = self.ice_tx.send(candidate);
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        eprintln!(
            "[{:>6.1}s] {} state {state}",
            self.started.elapsed().as_secs_f64(),
            self.name
        );
        let _ = self.state_tx.send((self.name, state));
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.dc_tx.send(dc);
    }
}

/// Pings over `dc` every 200 ms (so the relay side keeps calling `send_to`,
/// which is where rtc-turn refreshes channel bindings) and counts what arrives.
fn pump(dc: Arc<dyn DataChannel>, received: Arc<AtomicU64>) {
    let sender = Arc::clone(&dc);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if sender.send_text("ping").await.is_err() {
                // Not open yet, or closed; keep trying until the test ends.
            }
        }
    });
    tokio::spawn(async move {
        while let Some(event) = dc.poll().await {
            match event {
                DataChannelEvent::OnMessage(_) => {
                    received.fetch_add(1, Ordering::Relaxed);
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_session_outlives_turn_lifetimes() {
    let Ok(url) = std::env::var("RCDESK_TURN_SOAK_URL") else {
        eprintln!("RCDESK_TURN_SOAK_URL not set; skipping TURN soak");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let user = std::env::var("RCDESK_TURN_SOAK_USER").unwrap_or_default();
    let pass = std::env::var("RCDESK_TURN_SOAK_PASS").unwrap_or_default();
    let peer_ip = std::env::var("RCDESK_TURN_SOAK_PEER_IP").expect("RCDESK_TURN_SOAK_PEER_IP");
    let secs: u64 = std::env::var("RCDESK_TURN_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(700);

    let runtime = default_runtime().expect("runtime-tokio feature must be enabled");
    let started = Instant::now();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();

    // "Host": relay-only, like the sessions that drop at 600 s.
    let (relay_ice_tx, mut relay_ice_rx) = mpsc::unbounded_channel();
    let (relay_dc_tx, _relay_dc_rx) = mpsc::unbounded_channel();
    let relay_pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(
                RTCConfigurationBuilder::new()
                    .with_ice_servers(vec![RTCIceServer {
                        urls: vec![url],
                        username: user,
                        credential: pass,
                    }])
                    .with_ice_transport_policy(RTCIceTransportPolicy::Relay)
                    .build(),
            )
            .with_handler(Arc::new(Handler {
                name: "relay",
                started,
                ice_tx: relay_ice_tx,
                state_tx: state_tx.clone(),
                dc_tx: relay_dc_tx,
            }))
            .with_runtime(runtime.clone())
            .with_udp_addrs(vec!["0.0.0.0:0"])
            .build()
            .await
            .expect("relay peer"),
    );

    // "Browser": host candidates only, on an address the TURN server reaches.
    let (plain_ice_tx, mut plain_ice_rx) = mpsc::unbounded_channel();
    let (plain_dc_tx, mut plain_dc_rx) = mpsc::unbounded_channel();
    let plain_pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_handler(Arc::new(Handler {
                name: "plain",
                started,
                ice_tx: plain_ice_tx,
                state_tx,
                dc_tx: plain_dc_tx,
            }))
            .with_runtime(runtime.clone())
            .with_udp_addrs(vec![format!("{peer_ip}:0")])
            .build()
            .await
            .expect("plain peer"),
    );

    {
        let plain_pc = Arc::clone(&plain_pc);
        tokio::spawn(async move {
            while let Some(c) = relay_ice_rx.recv().await {
                let _ = plain_pc.add_ice_candidate(c).await;
            }
        });
    }
    {
        let relay_pc = Arc::clone(&relay_pc);
        tokio::spawn(async move {
            while let Some(c) = plain_ice_rx.recv().await {
                let _ = relay_pc.add_ice_candidate(c).await;
            }
        });
    }

    let relay_dc = relay_pc
        .create_data_channel("soak", None)
        .await
        .expect("data channel");
    let offer = relay_pc.create_offer(None).await.expect("offer");
    relay_pc
        .set_local_description(offer.clone())
        .await
        .expect("local offer");
    plain_pc
        .set_remote_description(RTCSessionDescription::offer(offer.sdp).expect("offer sdp"))
        .await
        .expect("remote offer");
    let answer = plain_pc.create_answer(None).await.expect("answer");
    plain_pc
        .set_local_description(answer.clone())
        .await
        .expect("local answer");
    relay_pc
        .set_remote_description(RTCSessionDescription::answer(answer.sdp).expect("answer sdp"))
        .await
        .expect("remote answer");

    let relay_received = Arc::new(AtomicU64::new(0));
    let plain_received = Arc::new(AtomicU64::new(0));
    pump(relay_dc, Arc::clone(&relay_received));
    {
        let plain_received = Arc::clone(&plain_received);
        tokio::spawn(async move {
            if let Some(dc) = plain_dc_rx.recv().await {
                pump(dc, plain_received);
            }
        });
    }

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut connected_at = None;
    let mut failure = None;
    let mut last_report = Instant::now();
    let mut last_counts = (0, 0);
    while Instant::now() < deadline {
        tokio::select! {
            Some((name, state)) = state_rx.recv() => {
                let t = started.elapsed().as_secs_f64();
                match state {
                    RTCPeerConnectionState::Connected if connected_at.is_none() => {
                        connected_at = Some(t);
                    }
                    RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Failed
                        if failure.is_none() =>
                    {
                        failure = Some((name, state, t));
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        if last_report.elapsed() >= Duration::from_secs(30) {
            let counts = (
                relay_received.load(Ordering::Relaxed),
                plain_received.load(Ordering::Relaxed),
            );
            eprintln!(
                "[{:>6.1}s] received relay={} (+{}) plain={} (+{})",
                started.elapsed().as_secs_f64(),
                counts.0,
                counts.0 - last_counts.0,
                counts.1,
                counts.1 - last_counts.1,
            );
            last_counts = counts;
            last_report = Instant::now();
        }
    }

    let _ = relay_pc.close().await;
    let _ = plain_pc.close().await;
    let connected_at = connected_at.expect("relay session never connected");
    assert!(
        failure.is_none(),
        "relay session connected at {connected_at:.1}s, then {failure:?}"
    );
}
