//! Binary entry point for the metering sidecar.
//!
//! Everything reusable lives in the library target; this file only reads a
//! configuration file, wires the edge together, and runs the listener.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

use std::process::ExitCode;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use mcp_usage_kit::{
    AggregatedUsage, BatchExporter, BillingPipeline, EdgeConfig, ExportError, ExportFuture,
    InMemoryTenantStore, LogExporter, MeterEventExporter, MeterLayer, SharedRecorder, TenantStore,
};
use tokio::net::TcpListener;
use tower::Layer;

use mcp_usage_edge::admission::AdmissionLayer;
use mcp_usage_edge::config::{Config, ExporterKind};
use mcp_usage_edge::control_plane::{
    ControlPlaneExporter, ControlPlaneTenantStore, PlaneClient, refresh_forever,
};
use mcp_usage_edge::mpp::{FacilitatorMethod, Payments, PaymentsConfig};
use mcp_usage_edge::proxy::{HttpsClient, UpstreamProxy, build_client};

const USAGE: &str = "usage: mcp-usage-edge <config.toml>";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Some(path) = config_path() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    match run(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Startup failures print as well as log, because an operator
            // watching a container start should not need a log level set.
            eprintln!("mcp-usage-edge: {error}");
            tracing::error!(%error, "sidecar failed");
            ExitCode::FAILURE
        }
    }
}

/// Accept either a bare path or `--config <path>`.
fn config_path() -> Option<String> {
    let mut args = std::env::args().skip(1);
    match args.next()?.as_str() {
        "--config" | "-c" => args.next(),
        "--help" | "-h" => {
            println!("{USAGE}");
            None
        }
        bare => Some(bare.to_owned()),
    }
}

fn run(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load(path)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(config))
}

/// Alias for the three related pieces the tenant source produces.
type TenantSource = (
    Arc<dyn TenantStore>,
    Option<PlaneClient>,
    Option<Arc<ControlPlaneTenantStore>>,
);

/// Build the tenant store, plus the plane client and cache when one is configured.
///
/// The two sources are mutually exclusive and the configuration already refused
/// anything ambiguous, so this only has to act on the choice.
fn tenant_source(
    config: &Config,
    http: &mcp_usage_edge::proxy::HttpsClient,
) -> Result<TenantSource, Box<dyn std::error::Error>> {
    if let Some(settings) = &config.control_plane {
        let client = PlaneClient::new(
            http.clone(),
            &settings.url,
            settings.token()?,
            settings.timeout(),
        )
        .with_max_response_bytes(settings.max_response_bytes);
        let store = Arc::new(ControlPlaneTenantStore::new(settings.max_stale()));
        return Ok((store.clone(), Some(client), Some(store)));
    }

    let store = Arc::new(InMemoryTenantStore::new());
    for (api_key, tenant) in config.resolve_tenants()? {
        let id = tenant.id.clone();
        // Strength was already checked while resolving, and re-checking here
        // would only re-derive the same verdict.
        store.insert_unchecked(&api_key, tenant);
        tracing::info!(tenant = %id, "registered tenant");
    }
    Ok((store, None, None))
}

/// Translate the `[edge]` table into an [`EdgeConfig`].
///
/// Every knob the library exposes through a builder is a named field in the
/// file, so this is a straight transcription with no defaulting of its own.
/// Connect the shared task store, when one is configured.
///
/// Without it the default store is process-local: a durable task created on
/// one instance and completed on another finds no attribution, and the
/// completing poll bills nothing at all.
#[cfg(feature = "redis")]
async fn task_store(
    config: &Config,
) -> Result<Option<Arc<dyn mcp_usage_kit::TaskAttributionStore>>, Box<dyn std::error::Error>> {
    let Some(settings) = &config.task_store else {
        return Ok(None);
    };
    let store = mcp_usage_kit::store::RedisTaskStore::connect_with_timeout(
        &settings.url()?,
        settings.key_prefix.clone(),
        settings.ttl(),
        settings.timeout(),
    )
    .await?;
    tracing::info!(
        ttl_seconds = settings.ttl_seconds,
        "durable task attribution is shared through Redis"
    );
    Ok(Some(Arc::new(store)))
}

/// Configuration already refused a `[task_store]` section in a build without
/// the feature, so there is nothing to connect here.
#[cfg(not(feature = "redis"))]
#[expect(
    clippy::unused_async,
    reason = "matches the signature of the redis-enabled variant"
)]
async fn task_store(
    _config: &Config,
) -> Result<Option<Arc<dyn mcp_usage_kit::TaskAttributionStore>>, Box<dyn std::error::Error>> {
    Ok(None)
}

fn edge_config(
    config: &Config,
    tenants: Arc<dyn TenantStore>,
    recorder: SharedRecorder,
    tasks: Option<Arc<dyn mcp_usage_kit::TaskAttributionStore>>,
) -> Result<EdgeConfig, Box<dyn std::error::Error>> {
    let mut edge = EdgeConfig::new(tenants);
    if let Some(tasks) = tasks {
        edge = edge.with_task_store(tasks);
    }
    let mut edge = edge
        .with_recorder(recorder)
        .with_strict_protocol_version(config.edge.strict_protocol_version)
        .with_credential_forwarding(config.edge.credential_forwarding);
    if let Some(bytes) = config.edge.max_request_body_bytes {
        edge = edge.with_max_request_body(bytes);
    }
    if let Some(bytes) = config.edge.max_response_capture_bytes {
        edge = edge.with_max_response_capture(bytes);
    }
    if let Some(name) = config.edge.meter_name.clone() {
        edge = edge.with_meter_name(name);
    }
    if let Some(limit) = &config.edge.auth_failure_limit {
        edge = edge.with_auth_failure_limit(limit.max_failures, limit.window())?;
    }
    if let Some(cache) = &config.edge.cache {
        edge = edge
            .with_cache(cache.max_entries, cache.max_ttl())
            .with_public_cache_sharing(cache.public_sharing);
    }
    Ok(edge)
}

/// Assemble the admission gate from configuration.
///
/// Quota and payments are independent: either, both, or neither. The layer is
/// always present so the sidecar has one service type regardless.
fn admission_gate(
    config: &Config,
    cached: Option<&Arc<ControlPlaneTenantStore>>,
    http: &HttpsClient,
) -> Result<AdmissionLayer, Box<dyn std::error::Error>> {
    let mut gate = AdmissionLayer::disabled();

    let enforce_quota = config
        .control_plane
        .as_ref()
        .is_some_and(|settings| settings.enforce_quota);
    if let (Some(store), true) = (cached, enforce_quota) {
        gate = gate.with_quota(store.clone());
    }

    if let Some(settings) = &config.mpp {
        let method = FacilitatorMethod::new(
            http.clone(),
            settings.method.clone(),
            settings.facilitator.url.clone(),
            settings.facilitator.token()?,
            settings.facilitator.timeout(),
        )
        .with_max_response_bytes(settings.facilitator.max_response_bytes);
        let payments = Payments::new(
            PaymentsConfig {
                secret: settings.secret()?,
                realm: settings.realm.clone(),
                intent: settings.intent.clone(),
                currency: settings.currency.clone(),
                recipient: settings.recipient.clone(),
                ttl: settings.challenge_ttl(),
                replay_capacity: settings.replay_capacity,
            },
            Box::new(method),
        );
        tracing::info!(
            method = %settings.method,
            realm = %settings.realm,
            intent = %settings.intent,
            "MPP payments enabled; an over-quota call is priced rather than refused"
        );
        gate = gate.with_payments(Arc::new(payments));
    }

    if let Some(bytes) = config.edge.max_request_body_bytes {
        gate = gate.with_max_body(bytes);
    }
    Ok(gate.with_strict_protocol_version(config.edge.strict_protocol_version))
}

async fn serve(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "tls")]
    install_crypto_provider();

    let http = build_client();
    let (tenants, plane, cached) = tenant_source(&config, &http)?;

    let exporter = EdgeExporter::configured(config.exporter.kind, plane.clone())?;
    let billing = Arc::new(BillingPipeline::new(exporter));

    let edge = edge_config(
        &config,
        tenants,
        billing.clone(),
        task_store(&config).await?,
    )?;

    if let (Some(store), Some(client), Some(settings)) = (
        cached.as_ref(),
        plane.as_ref(),
        config.control_plane.as_ref(),
    ) {
        start_refresh(store.clone(), client.clone(), settings.refresh_interval()).await;
    }

    let gate = admission_gate(&config, cached.as_ref(), &http)?;

    let proxy = UpstreamProxy::with_client(
        http.clone(),
        &config.upstream.url,
        config.upstream.timeout(),
    )?;
    let deferred = edge.deferred();
    let service = gate.layer(MeterLayer::new(edge).layer(proxy));

    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        listen = %config.listen,
        upstream = %config.upstream.url,
        credential_forwarding = config.edge.credential_forwarding,
        "metering sidecar ready"
    );

    let flusher = tokio::spawn(flush_loop(
        billing.clone(),
        deferred.clone(),
        config.exporter.flush_interval(),
    ));

    let (shutdown, stopping) = tokio::sync::watch::channel(false);
    let mut connections = tokio::task::JoinSet::new();
    let signal = shutdown_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result { tracing::warn!(%error, "connection task failed"); }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "accept failed");
                        continue;
                    }
                };
                let connection_service = TowerToHyperService::new(service.clone());
                let mut stopping = stopping.clone();
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let connection = http1::Builder::new().serve_connection(io, connection_service);
                    tokio::pin!(connection);
                    let result = tokio::select! {
                        result = &mut connection => result,
                        _ = stopping.changed() => {
                            connection.as_mut().graceful_shutdown();
                            connection.await
                        }
                    };
                    if let Err(error) = result {
                        tracing::debug!(%peer, %error, "connection closed");
                    }
                });
            }
            () = &mut signal => {
                tracing::info!("shutdown requested");
                break;
            }
        }
    }

    drop(listener);
    let _ = shutdown.send(true);
    finish_shutdown(
        connections,
        flusher,
        &billing,
        &deferred,
        std::time::Duration::from_secs(config.edge.shutdown_timeout_seconds),
    )
    .await
}

/// Load initial authentication data, then retry snapshots in the background.
async fn start_refresh(
    store: Arc<ControlPlaneTenantStore>,
    client: PlaneClient,
    interval: std::time::Duration,
) {
    match client.snapshot().await {
        Ok(snapshot) => {
            tracing::info!(keys = snapshot.tenants.len(), "loaded initial snapshot");
            store.apply(snapshot);
        }
        // A transient plane outage should not prevent the listener from starting.
        Err(error) => {
            tracing::error!(%error, "initial snapshot failed; starting cold and retrying");
        }
    }
    tokio::spawn(refresh_forever(store, client, interval));
}

async fn finish_shutdown(
    mut connections: tokio::task::JoinSet<()>,
    flusher: tokio::task::JoinHandle<()>,
    billing: &BillingPipeline<EdgeExporter>,
    deferred: &mcp_usage_kit::tower::DeferredCompletions,
    budget: std::time::Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now()
        .checked_add(budget)
        .ok_or("shutdown timeout is too large")?;
    flusher.abort();
    let finished = tokio::time::timeout_at(deadline, async {
        let _ = flusher.await;
        while let Some(result) = connections.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "connection task failed");
            }
        }
        deferred.drain().await;
        while billing.pending_buckets() > 0 {
            if let Err(error) = billing.flush().await {
                tracing::warn!(%error, "final flush failed; retrying within shutdown budget");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    })
    .await;
    report_unexported(billing);
    if finished.is_err() {
        tracing::error!(
            pending = billing.pending_buckets(),
            deferred = deferred.len(),
            active = connections.len(),
            "shutdown deadline exceeded; accounting may be incomplete"
        );
        return Err("shutdown deadline exceeded".into());
    }
    tracing::info!("final flush complete");
    Ok(())
}

async fn flush_loop(
    billing: Arc<BillingPipeline<EdgeExporter>>,
    deferred: Arc<mcp_usage_kit::tower::DeferredCompletions>,
    interval: std::time::Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        deferred.drain_some(deferred.len()).await;
        match billing.flush().await {
            Ok(0) | Err(ExportError::FlushInProgress) => {}
            Ok(count) => tracing::debug!(exported = count, "flushed usage"),
            // The pipeline restores the batch for retry, so the next tick
            // picks it up. Logging is all that is owed here.
            Err(error) => tracing::warn!(%error, "flush failed; batch retained for retry"),
        }
        report_unexported(&billing);
    }
}

/// Surface usage that is stuck or gone.
///
/// The exporter quarantines a permanently rejected aggregate into a bounded
/// queue and evicts the oldest when it fills. Nothing read that queue, so
/// revenue disappeared with no operator-visible signal at all. Draining it
/// here at least puts every discarded aggregate in the log, and a stuck retry
/// count is the difference between an invoice that is late and one that is
/// never coming.
fn report_unexported(billing: &BillingPipeline<EdgeExporter>) {
    let retained = billing.retry_buckets();
    if retained > 0 {
        tracing::warn!(
            retained,
            "usage the provider has not accepted is still buffered in memory; \
             it is lost if this process exits"
        );
    }

    let EdgeExporter::Plane(exporter) = billing.exporter() else {
        return;
    };
    let dropped = exporter.dropped_dead_letters();
    if dropped > 0 {
        tracing::error!(
            dropped,
            "reconciliation records were discarded because the dead letter \
             queue is full; this usage cannot be recovered"
        );
    }
    for letter in exporter.take_dead_letters() {
        tracing::error!(
            identifier = %letter.aggregate.identifier,
            customer_id = %letter.aggregate.customer_id,
            meter = %letter.aggregate.meter,
            units = letter.aggregate.units,
            reason = ?letter.reason,
            "usage was permanently rejected and needs reconciliation"
        );
    }
}

/// The two exporters a sidecar can be configured with.
///
/// `BillingPipeline` is generic over its exporter and `Arc<dyn BatchExporter>`
/// does not itself implement the trait, so the choice is a small enum rather
/// than a trait object.
enum EdgeExporter {
    Log(LogExporter),
    /// Boxed because it is an order of magnitude larger than the log variant,
    /// and this enum is moved into the pipeline by value.
    Plane(Box<MeterEventExporter<ControlPlaneExporter>>),
}

impl EdgeExporter {
    fn configured(kind: ExporterKind, plane: Option<PlaneClient>) -> Result<Self, &'static str> {
        match kind {
            ExporterKind::Log => Ok(Self::Log(LogExporter::new())),
            ExporterKind::ControlPlane => {
                let client = plane.ok_or(
                    "exporter.kind = \"control-plane\" requires a [control_plane] section",
                )?;
                Ok(Self::Plane(Box::new(MeterEventExporter::new(
                    ControlPlaneExporter::new(client),
                ))))
            }
        }
    }
}

impl BatchExporter for EdgeExporter {
    fn retry_policy(&self) -> mcp_usage_kit::export::RetryPolicy {
        match self {
            Self::Log(exporter) => exporter.retry_policy(),
            Self::Plane(exporter) => exporter.retry_policy(),
        }
    }

    fn export<'a>(&'a self, batch: &'a [AggregatedUsage]) -> ExportFuture<'a> {
        match self {
            Self::Log(exporter) => exporter.export(batch),
            Self::Plane(exporter) => exporter.export(batch),
        }
    }
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::warn!(%error, "cannot listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

#[cfg(feature = "tls")]
fn install_crypto_provider() {
    // Installing explicitly rather than relying on feature inference keeps the
    // failure mode at startup instead of on the first https upstream dial.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!("a rustls crypto provider was already installed");
    }
}
