//! An in-process iroh relay for tests (D106).
//!
//! `presets::N0` wires an endpoint to Number 0's PUBLIC relay and
//! discovery, and `endpoint.online()` blocks until that public
//! infrastructure answers — an internet dependency that hung a CI run
//! for the full 6-hour job cap when the runner couldn't reach n0. This
//! stands the same coordinator up on loopback: iroh's own
//! [`run_relay_server`] gives a relay on `127.0.0.1:0`, and endpoints
//! bound through [`TestNet`] dial each other over the relay-bearing
//! [`Endpoint::addr`]. The real iroh connection path runs — relay home
//! connection, `online()`, QUIC connect through the coordinator — but
//! net-less, so `online()` returns in milliseconds. This mirrors iroh's
//! own endpoint test suite; production keeps `presets::N0`.

use std::any::Any;

use anyhow::Result;
use iroh::endpoint::presets;
use iroh::test_utils::run_relay_server;
use iroh::tls::CaTlsConfig;
use iroh::{Endpoint, RelayMap, RelayMode};

/// A loopback iroh network — a relay running in this process. Endpoints
/// bound through it reach one another with no public-internet contact.
pub(crate) struct TestNet {
    relay_map: RelayMap,
    /// Held only to keep the relay's accept loop alive for the test's
    /// duration; dropping it shuts the relay down. Type-erased so the
    /// harness needn't name (and version-pin) `iroh_relay::server::Server`.
    _relay: Box<dyn Any + Send>,
}

impl TestNet {
    /// Spawn the loopback relay.
    pub(crate) async fn start() -> Result<Self> {
        let (relay_map, _url, relay) = run_relay_server().await?;
        Ok(Self {
            relay_map,
            _relay: Box::new(relay),
        })
    }

    /// Bind an endpoint on this network and wait for its home relay —
    /// fast, because the relay is on `127.0.0.1`. Peers connect to the
    /// returned endpoint's [`Endpoint::addr`], which carries the local
    /// relay url.
    pub(crate) async fn endpoint(&self) -> Result<Endpoint> {
        let endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Custom(self.relay_map.clone()))
            // The test relay serves a self-signed cert (D106).
            .ca_tls_config(CaTlsConfig::insecure_skip_verify())
            .bind()
            .await?;
        endpoint.online().await;
        Ok(endpoint)
    }
}
