use metrics::{CounterFn, GaugeFn, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use std::sync::Arc;
use vise::{Counter, Gauge, Metrics};

/// Metrics for the entire network, handled by `NetworkManager`.
///
/// This is a direct copy of [`reth_network::metrics::NetworkMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network")]
pub struct NetworkMetrics {
    /// Number of currently connected peers
    pub(crate) connected_peers: Gauge,
    /// Number of currently backed off peers
    pub(crate) backed_off_peers: Gauge,
    /// Number of peers known to the node
    pub(crate) tracked_peers: Gauge,
    /// Number of active incoming connections
    pub(crate) incoming_connections: Gauge,
    /// Number of active outgoing connections
    pub(crate) outgoing_connections: Gauge,
    /// Number of currently pending outgoing connections
    pub(crate) pending_outgoing_connections: Gauge,
    /// Total number of pending connections, incoming and outgoing
    pub(crate) total_pending_connections: Gauge,
    /// Total number of incoming connections handled
    pub(crate) total_incoming_connections: Counter,
    /// Total number of outgoing connections established
    pub(crate) total_outgoing_connections: Counter,
    /// Number of invalid/malformed messages received from peers
    pub(crate) invalid_messages_received: Counter,
    /// Number of Eth requests dropped due to channel being at full capacity
    pub(crate) total_dropped_eth_requests_at_full_capacity: Counter,
    /// Duration in seconds of call to NetworkManager's poll function
    pub(crate) duration_poll_network_manager: Gauge,
    /// Time spent streaming messages sent over the NetworkHandle in one poll call
    pub(crate) acc_duration_poll_network_handle: Gauge,
    /// Time spent polling Swarm in one poll call
    pub(crate) acc_duration_poll_swarm: Gauge,
}

/// Metrics for `SessionManager`.
///
/// This is a direct copy of [`reth_network::metrics::SessionManagerMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network")]
pub struct SessionManagerMetrics {
    /// Number of successful outgoing dial attempts
    pub(crate) total_dial_successes: Counter,
    /// Number of dropped outgoing peer messages
    pub(crate) total_outgoing_peer_messages_dropped: Counter,
    /// Number of queued outgoing messages
    pub(crate) queued_outgoing_messages: Gauge,
}

/// Metrics for backed-off peers, split by reason.
///
/// This is a direct copy of [`reth_network::metrics::BackedOffPeersMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network_backed_off_peers")]
pub struct BackedOffPeersMetrics {
    /// Peers backed off because they reported too many peers
    pub(crate) too_many_peers: Counter,
    /// Peers backed off after a graceful session close
    pub(crate) graceful_close: Counter,
    /// Peers backed off due to connection or protocol errors
    pub(crate) connection_error: Counter,
}

/// Metrics for closed sessions, split by direction.
///
/// This is a direct copy of [`reth_network::metrics::ClosedSessionsMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network_closed_sessions")]
pub struct ClosedSessionsMetrics {
    /// Sessions closed from active (established) connections
    pub(crate) active: Counter,
    /// Sessions closed from incoming pending connections
    pub(crate) incoming_pending: Counter,
    /// Sessions closed from outgoing pending connections
    pub(crate) outgoing_pending: Counter,
}

/// Metrics for pending session failures, split by direction.
///
/// This is a direct copy of [`reth_network::metrics::PendingSessionFailureMetrics`] but with
/// `vise` instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network_pending_session_failures")]
pub struct PendingSessionFailureMetrics {
    /// Failures on incoming pending sessions
    pub(crate) inbound: Counter,
    /// Failures on outgoing pending sessions
    pub(crate) outbound: Counter,
}

/// Metrics for peer disconnections.
///
/// This is a direct copy of [`reth_network::metrics::DisconnectMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network")]
pub struct DisconnectMetrics {
    /// Number of peer disconnects due to `DisconnectRequested`
    pub(crate) disconnect_requested: Counter,
    /// Number of peer disconnects due to `TcpSubsystemError`
    pub(crate) tcp_subsystem_error: Counter,
    /// Number of peer disconnects due to `ProtocolBreach`
    pub(crate) protocol_breach: Counter,
    /// Number of peer disconnects due to `UselessPeer`
    pub(crate) useless_peer: Counter,
    /// Number of peer disconnects due to `TooManyPeers`
    pub(crate) too_many_peers: Counter,
    /// Number of peer disconnects due to `AlreadyConnected`
    pub(crate) already_connected: Counter,
    /// Number of peer disconnects due to `IncompatibleP2PProtocolVersion`
    pub(crate) incompatible: Counter,
    /// Number of peer disconnects due to `NullNodeIdentity`
    pub(crate) null_node_identity: Counter,
    /// Number of peer disconnects due to `ClientQuitting`
    pub(crate) client_quitting: Counter,
    /// Number of peer disconnects due to `UnexpectedHandshakeIdentity`
    pub(crate) unexpected_identity: Counter,
    /// Number of peer disconnects due to `ConnectedToSelf`
    pub(crate) connected_to_self: Counter,
    /// Number of peer disconnects due to `PingTimeout`
    pub(crate) ping_timeout: Counter,
    /// Number of peer disconnects due to `SubprotocolSpecific`
    pub(crate) subprotocol_specific: Counter,
}

/// Metrics for inbound peer disconnections.
///
/// This is a direct copy of [`reth_network::metrics::InboundDisconnectMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network_inbound")]
pub struct InboundDisconnectMetrics {
    /// Number of inbound peer disconnects due to `DisconnectRequested`
    pub(crate) disconnect_requested: Counter,
    /// Number of inbound peer disconnects due to `TcpSubsystemError`
    pub(crate) tcp_subsystem_error: Counter,
    /// Number of inbound peer disconnects due to `ProtocolBreach`
    pub(crate) protocol_breach: Counter,
    /// Number of inbound peer disconnects due to `UselessPeer`
    pub(crate) useless_peer: Counter,
    /// Number of inbound peer disconnects due to `TooManyPeers`
    pub(crate) too_many_peers: Counter,
    /// Number of inbound peer disconnects due to `AlreadyConnected`
    pub(crate) already_connected: Counter,
    /// Number of inbound peer disconnects due to `IncompatibleP2PProtocolVersion`
    pub(crate) incompatible: Counter,
    /// Number of inbound peer disconnects due to `NullNodeIdentity`
    pub(crate) null_node_identity: Counter,
    /// Number of inbound peer disconnects due to `ClientQuitting`
    pub(crate) client_quitting: Counter,
    /// Number of inbound peer disconnects due to `UnexpectedHandshakeIdentity`
    pub(crate) unexpected_identity: Counter,
    /// Number of inbound peer disconnects due to `ConnectedToSelf`
    pub(crate) connected_to_self: Counter,
    /// Number of inbound peer disconnects due to `PingTimeout`
    pub(crate) ping_timeout: Counter,
    /// Number of inbound peer disconnects due to `SubprotocolSpecific`
    pub(crate) subprotocol_specific: Counter,
}

/// Metrics for outbound peer disconnections.
///
/// This is a direct copy of [`reth_network::metrics::OutboundDisconnectMetrics`] but with `vise`
/// instead of `metrics`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network_outbound")]
pub struct OutboundDisconnectMetrics {
    /// Number of outbound peer disconnects due to `DisconnectRequested`
    pub(crate) disconnect_requested: Counter,
    /// Number of outbound peer disconnects due to `TcpSubsystemError`
    pub(crate) tcp_subsystem_error: Counter,
    /// Number of outbound peer disconnects due to `ProtocolBreach`
    pub(crate) protocol_breach: Counter,
    /// Number of outbound peer disconnects due to `UselessPeer`
    pub(crate) useless_peer: Counter,
    /// Number of outbound peer disconnects due to `TooManyPeers`
    pub(crate) too_many_peers: Counter,
    /// Number of outbound peer disconnects due to `AlreadyConnected`
    pub(crate) already_connected: Counter,
    /// Number of outbound peer disconnects due to `IncompatibleP2PProtocolVersion`
    pub(crate) incompatible: Counter,
    /// Number of outbound peer disconnects due to `NullNodeIdentity`
    pub(crate) null_node_identity: Counter,
    /// Number of outbound peer disconnects due to `ClientQuitting`
    pub(crate) client_quitting: Counter,
    /// Number of outbound peer disconnects due to `UnexpectedHandshakeIdentity`
    pub(crate) unexpected_identity: Counter,
    /// Number of outbound peer disconnects due to `ConnectedToSelf`
    pub(crate) connected_to_self: Counter,
    /// Number of outbound peer disconnects due to `PingTimeout`
    pub(crate) ping_timeout: Counter,
    /// Number of outbound peer disconnects due to `SubprotocolSpecific`
    pub(crate) subprotocol_specific: Counter,
}

/// Metrics for peer discovery in `reth_discv5`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "discv5")]
pub struct Discv5Metrics {
    /// Total peers currently in discv5 kbuckets.
    pub(crate) kbucket_peers_raw_total: Gauge,
    /// Total peers currently connected to discv5.
    pub(crate) sessions_raw_total: Gauge,
    /// Total discovered peers inserted into discv5 kbuckets.
    pub(crate) inserted_kbucket_peers_raw_total: Counter,
    /// Total number of sessions established by discv5.
    pub(crate) established_sessions_raw_total: Counter,
    /// Total established sessions with peers that advertise an unreachable ENR.
    pub(crate) established_sessions_unreachable_enr_total: Counter,
    /// Total established sessions that pass configured filters.
    pub(crate) established_sessions_custom_filtered_total: Counter,
    /// Total unverifiable ENRs discovered by discv5.
    pub(crate) unverifiable_enrs_raw_total: Counter,
}

/// Frequency of networks advertised by discovered peers' node records.
#[derive(Debug, Metrics)]
#[metrics(prefix = "discv5")]
pub struct Discv5AdvertisedChainMetrics {
    /// Frequency of `opel` entries in discovered peers' ENRs.
    pub(crate) opel: Counter,
    /// Frequency of `opstack` entries in discovered peers' ENRs.
    pub(crate) opstack: Counter,
    /// Frequency of `eth` entries in discovered peers' ENRs.
    pub(crate) eth: Counter,
    /// Frequency of `eth2` entries in discovered peers' ENRs.
    pub(crate) eth2: Counter,
}

/// Throughput metrics emitted by `reth_metrics::common::mpsc::MeteredPollSender`.
#[derive(Debug, Metrics)]
#[metrics(prefix = "network_active_session")]
pub struct NetworkActiveSessionMetrics {
    /// Number of messages sent through the active-session channel.
    pub(crate) messages_sent_total: Counter,
    /// Number of delayed deliveries caused by back pressure.
    pub(crate) back_pressure_total: Counter,
}

/// Installs [`ViseRecorder`] as the global recorder for the `metrics` crate.
///
/// This bridges reth-network metrics (which use the `metrics` crate) to the `vise` collector.
/// Must be called before [`reth_network::NetworkManager`] is created, since that is when reth
/// registers its metric handles. If a global recorder is already set (e.g., from a prior call),
/// the error is logged and ignored — metrics may not be reported in that case.
///
/// Note: [`metrics::with_local_recorder`] (used by the mempool) takes priority over the global
/// recorder, so calling this does not interfere with other crates that use local recorders.
pub(crate) fn install_recorder() {
    if let Err(err) = ::metrics::set_global_recorder(ViseRecorder) {
        tracing::warn!(%err, "failed to install network metrics recorder; metrics may not be reported");
    }
}

#[vise::register]
pub(crate) static NETWORK_METRICS: vise::Global<NetworkMetrics> = vise::Global::new();
#[vise::register]
pub(crate) static SESSION_MANAGER_METRICS: vise::Global<SessionManagerMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static BACKED_OFF_PEERS_METRICS: vise::Global<BackedOffPeersMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static CLOSED_SESSIONS_METRICS: vise::Global<ClosedSessionsMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static PENDING_SESSION_FAILURE_METRICS: vise::Global<PendingSessionFailureMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static DISCONNECT_METRICS: vise::Global<DisconnectMetrics> = vise::Global::new();
#[vise::register]
pub(crate) static INBOUND_DISCONNECT_METRICS: vise::Global<InboundDisconnectMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static OUTBOUND_DISCONNECT_METRICS: vise::Global<OutboundDisconnectMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static DISCV5_METRICS: vise::Global<Discv5Metrics> = vise::Global::new();
#[vise::register]
pub(crate) static DISCV5_ADVERTISED_CHAIN_METRICS: vise::Global<Discv5AdvertisedChainMetrics> =
    vise::Global::new();
#[vise::register]
pub(crate) static NETWORK_ACTIVE_SESSION_METRICS: vise::Global<NetworkActiveSessionMetrics> =
    vise::Global::new();

/// A recorder that wraps `vise` metrics into `metrics`-compatible structs.
pub(crate) struct ViseRecorder;

impl Recorder for ViseRecorder {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        // Do nothing as descriptions are already provided by vise
    }

    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        // Do nothing as descriptions are already provided by vise
    }

    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        // Do nothing as descriptions are already provided by vise
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> metrics::Counter {
        let counter = match key.name() {
            // NetworkMetrics counters
            "network.total_incoming_connections" => &NETWORK_METRICS.total_incoming_connections,
            "network.total_outgoing_connections" => &NETWORK_METRICS.total_outgoing_connections,
            "network.invalid_messages_received" => &NETWORK_METRICS.invalid_messages_received,
            "network.total_dropped_eth_requests_at_full_capacity" => {
                &NETWORK_METRICS.total_dropped_eth_requests_at_full_capacity
            }
            // SessionManagerMetrics counters
            "network.total_dial_successes" => &SESSION_MANAGER_METRICS.total_dial_successes,
            "network.total_outgoing_peer_messages_dropped" => {
                &SESSION_MANAGER_METRICS.total_outgoing_peer_messages_dropped
            }
            // BackedOffPeersMetrics counters
            "network.backed_off_peers.too_many_peers" => &BACKED_OFF_PEERS_METRICS.too_many_peers,
            "network.backed_off_peers.graceful_close" => &BACKED_OFF_PEERS_METRICS.graceful_close,
            "network.backed_off_peers.connection_error" => {
                &BACKED_OFF_PEERS_METRICS.connection_error
            }
            // ClosedSessionsMetrics counters (labeled)
            "network_closed_sessions" => {
                let direction = key
                    .labels()
                    .find(|l| l.key() == "direction")
                    .map(|l| l.value());
                match direction {
                    Some("active") => &CLOSED_SESSIONS_METRICS.active,
                    Some("incoming_pending") => &CLOSED_SESSIONS_METRICS.incoming_pending,
                    Some("outgoing_pending") => &CLOSED_SESSIONS_METRICS.outgoing_pending,
                    _ => {
                        tracing::warn!(?key, "unknown network_closed_sessions direction label");
                        return metrics::Counter::noop();
                    }
                }
            }
            // PendingSessionFailureMetrics counters (labeled)
            "network_pending_session_failures" => {
                let direction = key
                    .labels()
                    .find(|l| l.key() == "direction")
                    .map(|l| l.value());
                match direction {
                    Some("inbound") => &PENDING_SESSION_FAILURE_METRICS.inbound,
                    Some("outbound") => &PENDING_SESSION_FAILURE_METRICS.outbound,
                    _ => {
                        tracing::warn!(
                            ?key,
                            "unknown network_pending_session_failures direction label"
                        );
                        return metrics::Counter::noop();
                    }
                }
            }
            // DisconnectMetrics counters
            "network.disconnect_requested" => &DISCONNECT_METRICS.disconnect_requested,
            "network.tcp_subsystem_error" => &DISCONNECT_METRICS.tcp_subsystem_error,
            "network.protocol_breach" => &DISCONNECT_METRICS.protocol_breach,
            "network.useless_peer" => &DISCONNECT_METRICS.useless_peer,
            "network.too_many_peers" => &DISCONNECT_METRICS.too_many_peers,
            "network.already_connected" => &DISCONNECT_METRICS.already_connected,
            "network.incompatible" => &DISCONNECT_METRICS.incompatible,
            "network.null_node_identity" => &DISCONNECT_METRICS.null_node_identity,
            "network.client_quitting" => &DISCONNECT_METRICS.client_quitting,
            "network.unexpected_identity" => &DISCONNECT_METRICS.unexpected_identity,
            "network.connected_to_self" => &DISCONNECT_METRICS.connected_to_self,
            "network.ping_timeout" => &DISCONNECT_METRICS.ping_timeout,
            "network.subprotocol_specific" => &DISCONNECT_METRICS.subprotocol_specific,
            // InboundDisconnectMetrics counters
            "network.inbound.disconnect_requested" => {
                &INBOUND_DISCONNECT_METRICS.disconnect_requested
            }
            "network.inbound.tcp_subsystem_error" => {
                &INBOUND_DISCONNECT_METRICS.tcp_subsystem_error
            }
            "network.inbound.protocol_breach" => &INBOUND_DISCONNECT_METRICS.protocol_breach,
            "network.inbound.useless_peer" => &INBOUND_DISCONNECT_METRICS.useless_peer,
            "network.inbound.too_many_peers" => &INBOUND_DISCONNECT_METRICS.too_many_peers,
            "network.inbound.already_connected" => &INBOUND_DISCONNECT_METRICS.already_connected,
            "network.inbound.incompatible" => &INBOUND_DISCONNECT_METRICS.incompatible,
            "network.inbound.null_node_identity" => &INBOUND_DISCONNECT_METRICS.null_node_identity,
            "network.inbound.client_quitting" => &INBOUND_DISCONNECT_METRICS.client_quitting,
            "network.inbound.unexpected_identity" => {
                &INBOUND_DISCONNECT_METRICS.unexpected_identity
            }
            "network.inbound.connected_to_self" => &INBOUND_DISCONNECT_METRICS.connected_to_self,
            "network.inbound.ping_timeout" => &INBOUND_DISCONNECT_METRICS.ping_timeout,
            "network.inbound.subprotocol_specific" => {
                &INBOUND_DISCONNECT_METRICS.subprotocol_specific
            }
            // OutboundDisconnectMetrics counters
            "network.outbound.disconnect_requested" => {
                &OUTBOUND_DISCONNECT_METRICS.disconnect_requested
            }
            "network.outbound.tcp_subsystem_error" => {
                &OUTBOUND_DISCONNECT_METRICS.tcp_subsystem_error
            }
            "network.outbound.protocol_breach" => &OUTBOUND_DISCONNECT_METRICS.protocol_breach,
            "network.outbound.useless_peer" => &OUTBOUND_DISCONNECT_METRICS.useless_peer,
            "network.outbound.too_many_peers" => &OUTBOUND_DISCONNECT_METRICS.too_many_peers,
            "network.outbound.already_connected" => &OUTBOUND_DISCONNECT_METRICS.already_connected,
            "network.outbound.incompatible" => &OUTBOUND_DISCONNECT_METRICS.incompatible,
            "network.outbound.null_node_identity" => {
                &OUTBOUND_DISCONNECT_METRICS.null_node_identity
            }
            "network.outbound.client_quitting" => &OUTBOUND_DISCONNECT_METRICS.client_quitting,
            "network.outbound.unexpected_identity" => {
                &OUTBOUND_DISCONNECT_METRICS.unexpected_identity
            }
            "network.outbound.connected_to_self" => &OUTBOUND_DISCONNECT_METRICS.connected_to_self,
            "network.outbound.ping_timeout" => &OUTBOUND_DISCONNECT_METRICS.ping_timeout,
            "network.outbound.subprotocol_specific" => {
                &OUTBOUND_DISCONNECT_METRICS.subprotocol_specific
            }
            // Discv5 counters
            "discv5.inserted_kbucket_peers_raw_total" => {
                &DISCV5_METRICS.inserted_kbucket_peers_raw_total
            }
            "discv5.established_sessions_raw_total" => {
                &DISCV5_METRICS.established_sessions_raw_total
            }
            "discv5.established_sessions_unreachable_enr_total" => {
                &DISCV5_METRICS.established_sessions_unreachable_enr_total
            }
            "discv5.established_sessions_custom_filtered_total" => {
                &DISCV5_METRICS.established_sessions_custom_filtered_total
            }
            "discv5.unverifiable_enrs_raw_total" => &DISCV5_METRICS.unverifiable_enrs_raw_total,
            "discv5.opel" => &DISCV5_ADVERTISED_CHAIN_METRICS.opel,
            "discv5.opstack" => &DISCV5_ADVERTISED_CHAIN_METRICS.opstack,
            "discv5.eth" => &DISCV5_ADVERTISED_CHAIN_METRICS.eth,
            "discv5.eth2" => &DISCV5_ADVERTISED_CHAIN_METRICS.eth2,
            // Dynamic `MeteredPollSender` counters used by active sessions
            "network_active_session.messages_sent_total" => {
                &NETWORK_ACTIVE_SESSION_METRICS.messages_sent_total
            }
            "network_active_session.back_pressure_total" => {
                &NETWORK_ACTIVE_SESSION_METRICS.back_pressure_total
            }
            _ => {
                tracing::warn!(?key, "unknown counter metric");
                return metrics::Counter::noop();
            }
        };
        metrics::Counter::from_arc(Arc::new(ViseCounter(counter.clone())))
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> metrics::Gauge {
        let gauge = match key.name() {
            // NetworkMetrics gauges
            "network.connected_peers" => &NETWORK_METRICS.connected_peers,
            "network.backed_off_peers" => &NETWORK_METRICS.backed_off_peers,
            "network.tracked_peers" => &NETWORK_METRICS.tracked_peers,
            "network.incoming_connections" => &NETWORK_METRICS.incoming_connections,
            "network.outgoing_connections" => &NETWORK_METRICS.outgoing_connections,
            "network.pending_outgoing_connections" => &NETWORK_METRICS.pending_outgoing_connections,
            "network.total_pending_connections" => &NETWORK_METRICS.total_pending_connections,
            "network.duration_poll_network_manager" => {
                &NETWORK_METRICS.duration_poll_network_manager
            }
            "network.acc_duration_poll_network_handle" => {
                &NETWORK_METRICS.acc_duration_poll_network_handle
            }
            "network.acc_duration_poll_swarm" => &NETWORK_METRICS.acc_duration_poll_swarm,
            // SessionManagerMetrics gauges
            "network.queued_outgoing_messages" => &SESSION_MANAGER_METRICS.queued_outgoing_messages,
            // Discv5 gauges
            "discv5.kbucket_peers_raw_total" => &DISCV5_METRICS.kbucket_peers_raw_total,
            "discv5.sessions_raw_total" => &DISCV5_METRICS.sessions_raw_total,
            _ => {
                tracing::warn!(?key, "unknown gauge metric");
                return metrics::Gauge::noop();
            }
        };
        metrics::Gauge::from_arc(Arc::new(ViseGauge(gauge.clone())))
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> metrics::Histogram {
        tracing::warn!(?key, "unknown histogram metric");
        metrics::Histogram::noop()
    }
}

/// A wrapper around `vise::Counter` that implements `metrics::CounterFn`.
struct ViseCounter(Counter);

impl CounterFn for ViseCounter {
    fn increment(&self, value: u64) {
        self.0.inc_by(value);
    }

    fn absolute(&self, _value: u64) {
        tracing::warn!("tried to set metric counter to absolute value; this is not supported");
    }
}

/// A wrapper around `vise::Gauge` that implements `metrics::GaugeFn`.
struct ViseGauge(Gauge);

impl GaugeFn for ViseGauge {
    fn increment(&self, value: f64) {
        self.0.inc_by(value.floor() as i64);
    }

    fn decrement(&self, value: f64) {
        self.0.dec_by(value.floor() as i64);
    }

    fn set(&self, value: f64) {
        self.0.set(value.floor() as i64);
    }
}
