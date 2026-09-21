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

use mcp_usage_edge::config::{Config, ExporterKind};
use mcp_usage_edge::control_plane::{
    ControlPlaneExporter, ControlPlaneTenantStore, PlaneClient, refresh_forever,
};
use mcp_usage_edge::proxy::{UpstreamProxy, build_client};
use mcp_usage_edge::quota::QuotaLayer;

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
            tracing::error!(%error, "startup failed");
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
        );
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
fn edge_config(
    config: &Config,
    tenants: Arc<dyn TenantStore>,
    recorder: SharedRecorder,
) -> Result<EdgeConfig, Box<dyn std::error::Error>> {
    let mut edge = EdgeConfig::new(tenants)
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

async fn serve(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "tls")]
    install_crypto_provider();

    let http = build_client();
    let (tenants, plane, cached) = tenant_source(&config, &http)?;

    let exporter = match config.exporter.kind {
        ExporterKind::Log => EdgeExporter::Log(LogExporter::new()),
        ExporterKind::ControlPlane => {
            let client = plane
                .clone()
                .ok_or("exporter.kind = \"control-plane\" requires a [control_plane] section")?;
            EdgeExporter::Plane(Box::new(MeterEventExporter::new(
                ControlPlaneExporter::new(client),
            )))
        }
    };
    let billing = Arc::new(BillingPipeline::new(exporter));

    let edge = edge_config(&config, tenants, billing.clone())?;

    // Pull one snapshot before the listener opens, so the sidecar does not
    // refuse every call for the first refresh interval.
    if let (Some(store), Some(client)) = (cached.as_ref(), plane.as_ref()) {
        match client.snapshot().await {
            Ok(snapshot) => {
                tracing::info!(keys = snapshot.tenants.len(), "loaded initial snapshot");
                store.apply(snapshot);
            }
            // Starting anyway is deliberate: the refresh loop retries, and a
            // sidecar that refuses to boot because the plane is briefly down is
            // an outage the plane should not be able to cause.
            Err(error) => {
                tracing::error!(%error, "initial snapshot failed; starting cold and retrying");
            }
        }
        let settings = config
            .control_plane
            .as_ref()
            .expect("plane implies settings");
        tokio::spawn(refresh_forever(
            store.clone(),
            client.clone(),
            settings.refresh_interval(),
        ));
    }

    let enforce_quota = config
        .control_plane
        .as_ref()
        .is_some_and(|settings| settings.enforce_quota);
    let gate = match (&cached, enforce_quota) {
        (Some(store), true) => QuotaLayer::new(store.clone()),
        // Quota needs authoritative counters, which a statically configured
        // sidecar does not have.
        _ => QuotaLayer::disabled(),
    };

    let proxy = UpstreamProxy::with_client(http, &config.upstream.url, config.upstream.timeout())?;
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
        config.exporter.flush_interval(),
    ));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "accept failed");
                        continue;
                    }
                };
                let connection_service = TowerToHyperService::new(service.clone());
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    if let Err(error) = http1::Builder::new()
                        .serve_connection(io, connection_service)
                        .await
                    {
                        tracing::debug!(%peer, %error, "connection closed");
                    }
                });
            }
            () = shutdown_signal() => {
                tracing::info!("shutdown requested");
                break;
            }
        }
    }

    // Usage already recorded but not yet exported would otherwise be lost, so
    // the final flush runs before the process exits rather than after the
    // flush task is dropped.
    flusher.abort();
    match billing.flush().await {
        Ok(count) => tracing::info!(exported = count, "final flush complete"),
        Err(error) => tracing::error!(%error, "final flush failed; usage may be unexported"),
    }
    Ok(())
}

async fn flush_loop(billing: Arc<BillingPipeline<EdgeExporter>>, interval: std::time::Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match billing.flush().await {
            Ok(0) | Err(ExportError::FlushInProgress) => {}
            Ok(count) => tracing::debug!(exported = count, "flushed usage"),
            // The pipeline restores the batch for retry, so the next tick
            // picks it up. Logging is all that is owed here.
            Err(error) => tracing::warn!(%error, "flush failed; batch retained for retry"),
        }
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

impl BatchExporter for EdgeExporter {
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
