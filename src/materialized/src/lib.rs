// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A SQL stream processor built on top of [timely dataflow] and
//! [differential dataflow].
//!
//! [differential dataflow]: ../differential_dataflow/index.html
//! [timely dataflow]: ../timely/index.html

use std::convert::TryInto;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use compile_time_run::run_command_str;
use futures::StreamExt;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
use ore::{
    metric,
    metrics::{Gauge, MetricsRegistry, UIntGauge, UIntGaugeVec},
};
use sysinfo::{ProcessorExt, SystemExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;

use build_info::BuildInfo;
use coord::LoggingConfig;

use crate::mux::Mux;

mod http;
mod mux;
mod server_metrics;
mod telemetry;

// Disable jemalloc on macOS, as it is not well supported [0][1][2].
// The issues present as runaway latency on load test workloads that are
// comfortably handled by the macOS system allocator. Consider re-evaluating if
// jemalloc's macOS support improves.
//
// [0]: https://github.com/jemalloc/jemalloc/issues/26
// [1]: https://github.com/jemalloc/jemalloc/issues/843
// [2]: https://github.com/jemalloc/jemalloc/issues/1467
#[cfg(not(target_os = "macos"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub const BUILD_INFO: BuildInfo = BuildInfo {
    version: env!("CARGO_PKG_VERSION"),
    sha: run_command_str!(
        "sh",
        "-c",
        r#"if [ -n "$MZ_DEV_BUILD_SHA" ]; then
            echo "$MZ_DEV_BUILD_SHA"
        else
            # Unfortunately we need to suppress error messages from `git`, as
            # run_command_str will display no error message at all if we print
            # more than one line of output to stderr.
            git rev-parse --verify HEAD 2>/dev/null || {
                printf "error: unable to determine Git SHA; " >&2
                printf "either build from working Git clone " >&2
                printf "(see https://materialize.com/docs/install/#build-from-source), " >&2
                printf "or specify SHA manually by setting MZ_DEV_BUILD_SHA environment variable" >&2
                exit 1
            }
        fi"#
    ),
    time: run_command_str!("date", "-u", "+%Y-%m-%dT%H:%M:%SZ"),
    target_triple: env!("TARGET_TRIPLE"),
};

/// Configuration for a `materialized` server.
#[derive(Debug, Clone)]
pub struct Config {
    // === Timely and Differential worker options. ===
    /// The number of Timely worker threads that this process should host.
    pub workers: usize,
    /// The Timely worker configuration.
    pub timely_worker: timely::WorkerConfig,

    // === Performance tuning options. ===
    pub logging: Option<LoggingConfig>,
    /// The frequency at which to update introspection.
    pub introspection_frequency: Duration,
    /// The historical window in which distinctions are maintained for
    /// arrangements.
    ///
    /// As arrangements accept new timestamps they may optionally collapse prior
    /// timestamps to the same value, retaining their effect but removing their
    /// distinction. A large value or `None` results in a large amount of
    /// historical detail for arrangements; this increases the logical times at
    /// which they can be accurately queried, but consumes more memory. A low
    /// value reduces the amount of memory required but also risks not being
    /// able to use the arrangement in a query that has other constraints on the
    /// timestamps used (e.g. when joined with other arrangements).
    pub logical_compaction_window: Option<Duration>,
    /// The interval at which sources should be timestamped.
    pub timestamp_frequency: Duration,

    // === Connection options. ===
    /// The IP address and port to listen on.
    pub listen_addr: SocketAddr,
    /// TLS encryption configuration.
    pub tls: Option<TlsConfig>,

    // === Storage options. ===
    /// The directory in which `materialized` should store its own metadata.
    pub data_directory: PathBuf,

    // === Mode switches. ===
    /// An optional symbiosis endpoint. See the
    /// [`symbiosis`](../symbiosis/index.html) crate for details.
    pub symbiosis_url: Option<String>,
    /// Whether to permit usage of experimental features.
    pub experimental_mode: bool,
    /// Whether to run in safe mode.
    pub safe_mode: bool,
    /// Telemetry configuration.
    pub telemetry: Option<TelemetryConfig>,
    /// The place where the server's metrics will be reported from.
    pub metrics_registry: MetricsRegistry,
}

/// Configures TLS encryption for connections.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// The TLS mode to use.
    pub mode: TlsMode,
    /// The path to the TLS certificate.
    pub cert: PathBuf,
    /// The path to the TLS key.
    pub key: PathBuf,
}

/// Configures how strictly to enforce TLS encryption and authentication.
#[derive(Debug, Clone)]
pub enum TlsMode {
    /// Require that all clients connect with TLS, but do not require that they
    /// present a client certificate.
    Require,
    /// Require that clients connect with TLS and present a certificate that
    /// is signed by the specified CA.
    VerifyCa {
        /// The path to a TLS certificate authority.
        ca: PathBuf,
    },
    /// Like [`TlsMode::VerifyCa`], but the `cn` (Common Name) field of the
    /// certificate must additionally match the user named in the connection
    /// request.
    VerifyFull {
        /// The path to a TLS certificate authority.
        ca: PathBuf,
    },
}

/// Telemetry configuration.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// The domain hosting the telemetry server.
    pub domain: String,
    /// The interval at which to report telemetry data.
    pub interval: Duration,
}

/// Global metrics for the materialized server
#[derive(Debug, Clone)]
pub struct Metrics {
    /// The number of workers active in the system.
    worker_count: UIntGaugeVec,

    /// The number of seconds that the system has been running.
    uptime: Gauge,

    /// The amount of time we spend gathering metrics in prometheus endpoints.
    request_metrics_gather: UIntGauge,

    /// The amount of time we spend encoding metrics in prometheus endpoints.
    request_metrics_encode: UIntGauge,
}

impl Metrics {
    fn register_with(registry: &MetricsRegistry) -> Self {
        let mut system = sysinfo::System::new();
        system.refresh_system();

        let request_metrics: UIntGaugeVec = registry.register(metric!(
            name: "mz_server_scrape_metrics_times",
            help: "how long it took to gather metrics, used for very low frequency high accuracy measures",
            var_labels: ["action"],
        ));
        Self {
            worker_count: registry.register(metric!(
                name: "mz_server_metadata_timely_worker_threads",
                help: "number of timely worker threads",
                var_labels: ["count"],
            )),
            uptime: registry.register(metric!(
                name: "mz_server_metadata_seconds",
                help: "server metadata, value is uptime",
                const_labels: {
                    "build_time" => BUILD_INFO.time,
                    "version" => BUILD_INFO.version,
                    "build_sha" => BUILD_INFO.sha,
                    "os" => &os_info::get().to_string(),
                    "ncpus_logical" => &num_cpus::get().to_string(),
                    "ncpus_physical" => &num_cpus::get_physical().to_string(),
                    "cpu0" => &{
                        match &system.processors().get(0) {
                            None => "<unknown>".to_string(),
                            Some(cpu0) => format!("{} {}MHz", cpu0.brand(), cpu0.frequency()),
                        }
                    },
                    "memory_total" => &system.total_memory().to_string()
                },
            )),
            request_metrics_gather: request_metrics.with_label_values(&["gather"]),
            request_metrics_encode: request_metrics.with_label_values(&["encode"]),
        }
    }

    fn update_uptime(&self, start_time: Instant) {
        let uptime = start_time.elapsed();
        let (secs, milli_part) = (uptime.as_secs() as f64, uptime.subsec_millis() as f64);
        self.uptime.set(secs + milli_part / 1_000.0);
    }
}

/// Start a `materialized` server.
pub async fn serve(config: Config) -> Result<Server, anyhow::Error> {
    let workers = config.workers;

    // Validate TLS configuration, if present.
    let (pgwire_tls, http_tls) = match &config.tls {
        None => (None, None),
        Some(tls_config) => {
            let context = {
                // Mozilla publishes three presets: old, intermediate, and modern. They
                // recommend the intermediate preset for general purpose servers, which
                // is what we use, as it is compatible with nearly every client released
                // in the last five years but does not include any known-problematic
                // ciphers. We once tried to use the modern preset, but it was
                // incompatible with Fivetran, and presumably other JDBC-based tools.
                let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
                if let TlsMode::VerifyCa { ca } | TlsMode::VerifyFull { ca } = &tls_config.mode {
                    builder.set_ca_file(ca)?;
                    builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
                }
                builder.set_certificate_file(&tls_config.cert, SslFiletype::PEM)?;
                builder.set_private_key_file(&tls_config.key, SslFiletype::PEM)?;
                builder.build().into_context()
            };
            let pgwire_tls = pgwire::TlsConfig {
                context: context.clone(),
                mode: match tls_config.mode {
                    TlsMode::Require | TlsMode::VerifyCa { .. } => pgwire::TlsMode::Require,
                    TlsMode::VerifyFull { .. } => pgwire::TlsMode::VerifyUser,
                },
            };
            let http_tls = http::TlsConfig {
                context,
                mode: match tls_config.mode {
                    TlsMode::Require | TlsMode::VerifyCa { .. } => http::TlsMode::Require,
                    TlsMode::VerifyFull { .. } => http::TlsMode::AssumeUser,
                },
            };
            (Some(pgwire_tls), Some(http_tls))
        }
    };
    let metrics_registry = config.metrics_registry;
    let metrics = Metrics::register_with(&metrics_registry);

    // Set this metric once so that it shows up in the metric export.
    metrics
        .worker_count
        .with_label_values(&[&workers.to_string()])
        .set(workers.try_into().unwrap());

    // Initialize network listener.
    let listener = TcpListener::bind(&config.listen_addr).await?;
    let local_addr = listener.local_addr()?;

    // Initialize coordinator.
    let (coord_handle, coord_client) = coord::serve(coord::Config {
        workers,
        timely_worker: config.timely_worker,
        symbiosis_url: config.symbiosis_url.as_deref(),
        logging: config.logging,
        data_directory: &config.data_directory,
        timestamp_frequency: config.timestamp_frequency,
        logical_compaction_window: config.logical_compaction_window,
        experimental_mode: config.experimental_mode,
        safe_mode: config.safe_mode,
        build_info: &BUILD_INFO,
        metrics_registry: metrics_registry.clone(),
    })
    .await?;

    // Launch task to serve connections.
    //
    // The lifetime of this task is controlled by a trigger that activates on
    // drop. Draining marks the beginning of the server shutdown process and
    // indicates that new user connections (i.e., pgwire and HTTP connections)
    // should be rejected. Once all existing user connections have gracefully
    // terminated, this task exits.
    let (drain_trigger, drain_tripwire) = oneshot::channel();
    tokio::spawn({
        let mut mux = Mux::new();
        mux.add_handler(pgwire::Server::new(pgwire::Config {
            tls: pgwire_tls,
            coord_client: coord_client.clone(),
            metrics_registry: &metrics_registry,
        }));
        mux.add_handler(http::Server::new(http::Config {
            tls: http_tls,
            coord_client: coord_client.clone(),
            start_time: coord_handle.start_instant(),
            metrics_registry: metrics_registry.clone(),
            global_metrics: metrics.clone(),
        }));
        async move {
            // TODO(benesch): replace with `listener.incoming()` if that is
            // restored when the `Stream` trait stabilizes.
            let mut incoming = TcpListenerStream::new(listener);
            mux.serve(incoming.by_ref().take_until(drain_tripwire))
                .await;
        }
    });

    tokio::spawn({
        let start_time = coord_handle.start_instant();
        let frequency = config.introspection_frequency;
        async move {
            loop {
                metrics.update_uptime(start_time);
                tokio::time::sleep(frequency).await;
            }
        }
    });

    // Start telemetry reporting loop.
    if let Some(telemetry) = config.telemetry {
        let config = telemetry::Config {
            domain: telemetry.domain,
            interval: telemetry.interval,
            cluster_id: coord_handle.cluster_id(),
            coord_client,
        };
        tokio::spawn(async move { telemetry::report_loop(config).await });
    }

    Ok(Server {
        local_addr,
        _drain_trigger: drain_trigger,
        _coord_handle: coord_handle,
    })
}

/// A running `materialized` server.
pub struct Server {
    local_addr: SocketAddr,
    // Drop order matters for these fields.
    _drain_trigger: oneshot::Sender<()>,
    _coord_handle: coord::Handle,
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}
