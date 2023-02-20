use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(feature = "mwal_backend")]
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as AnyhowContext;
use database::libsql::LibSqlDb;
use database::service::DbFactoryService;
use database::write_proxy::WriteProxyDbFactory;
use futures::pin_mut;
use once_cell::sync::Lazy;
#[cfg(feature = "mwal_backend")]
use once_cell::sync::OnceCell;
use replication::logger::{ReplicationLogger, ReplicationLoggerHook};
use rpc::run_rpc_server;
use system::System;
use tokio::sync::Notify;
use tower::load::Constant;
use tower::ServiceExt;

use crate::error::Error;
use crate::postgres::service::PgConnectionFactory;
use crate::server::Server;

pub use sqld_libsql_bindings as libsql;

mod database;
mod error;
mod http;
mod postgres;
mod query;
mod query_analysis;
mod replication;
pub mod rpc;
mod server;
mod system;

#[derive(clap::ValueEnum, Clone, Debug, PartialEq)]
pub enum Backend {
    Libsql,
    #[cfg(feature = "mwal_backend")]
    Mwal,
}

type Result<T> = std::result::Result<T, Error>;

/// Trigger a hard database reset. This cause the database to be wiped, freshly restarted
/// This is used for replicas that are left in an unrecoverabe state and should restart from a
/// fresh state.
///
/// /!\ use with caution.
pub(crate) static HARD_RESET: Lazy<Arc<Notify>> = Lazy::new(|| Arc::new(Notify::new()));
/// Clean shutdown of the server.
pub(crate) static SHUTDOWN: Lazy<Arc<Notify>> = Lazy::new(|| Arc::new(Notify::new()));

#[cfg(feature = "mwal_backend")]
pub(crate) static VWAL_METHODS: OnceCell<
    Option<Arc<Mutex<sqld_libsql_bindings::mwal::ffi::libsql_wal_methods>>>,
> = OnceCell::new();

pub struct Config {
    pub db_path: PathBuf,
    pub tcp_addr: Option<SocketAddr>,
    pub ws_addr: Option<SocketAddr>,
    pub http_addr: Option<SocketAddr>,
    pub http_auth: Option<String>,
    pub enable_http_console: bool,
    pub backend: Backend,
    #[cfg(feature = "mwal_backend")]
    pub mwal_addr: Option<String>,
    pub writer_rpc_addr: Option<String>,
    pub writer_rpc_tls: bool,
    pub writer_rpc_cert: Option<PathBuf>,
    pub writer_rpc_key: Option<PathBuf>,
    pub writer_rpc_ca_cert: Option<PathBuf>,
    pub rpc_server_addr: Option<SocketAddr>,
    pub rpc_server_tls: bool,
    pub rpc_server_cert: Option<PathBuf>,
    pub rpc_server_key: Option<PathBuf>,
    pub rpc_server_ca_cert: Option<PathBuf>,
    pub enable_bottomless_replication: bool,
    pub create_local_http_tunnel: bool,
    pub idle_shutdown_timeout: Option<Duration>,
}

async fn run_service(
    service: DbFactoryService,
    config: &Config,
    system: &System,
) -> anyhow::Result<()> {
    let mut server = Server::new();

    if let Some(addr) = config.tcp_addr {
        server.bind_tcp(addr).await?;
    }

    if let Some(addr) = config.ws_addr {
        server.bind_ws(addr).await?;
    }

    if config.ws_addr.is_some() || config.tcp_addr.is_some() {
        let factory = PgConnectionFactory::new(service.clone());
        system.register(server.serve(factory), "pgwire server");
    }

    if let Some(addr) = config.http_addr {
        let authorizer = http::auth::parse_auth(config.http_auth.clone())
            .context("failed to parse HTTP auth config")?;
        system.register(
            http::run_http(
                addr,
                authorizer,
                service.map_response(|s| Constant::new(s, 1)),
                config.enable_http_console,
                config.idle_shutdown_timeout,
            ),
            "http server",
        );
    }

    Ok(())
}

/// nukes current DB and start anew
async fn hard_reset(config: &Config, mut system: Pin<&mut System>) -> anyhow::Result<()> {
    tracing::error!("received hard-reset command: reseting replica.");

    tracing::info!("Shutting down all services...");
    system.shutdown();
    pin_mut!(system);
    let _ = system.await;

    tracing::info!("All services have been shut down.");

    let db_path = &config.db_path;
    tokio::fs::remove_dir_all(db_path).await?;

    Ok(())
}

async fn start_primary(config: &Config, system: &System, addr: &str) -> anyhow::Result<()> {
    let (factory, handle) = WriteProxyDbFactory::new(
        addr,
        config.writer_rpc_tls,
        config.writer_rpc_cert.clone(),
        config.writer_rpc_key.clone(),
        config.writer_rpc_ca_cert.clone(),
        config.db_path.clone(),
    )
    .await
    .context("failed to start WriteProxy DB")?;

    // the `JoinSet` does not support inserting arbitrary `JoinHandles` (and it also does not
    // support spawning blocking tasks, so we must spawn a proxy task to add the `handle` to
    // `join_set`
    system.register(
        async move { handle.await.expect("WriteProxy DB task failed") },
        "write proxy db task",
    );

    let service = DbFactoryService::new(Arc::new(factory));
    run_service(service, config, system).await?;

    Ok(())
}

async fn start_replica(config: &Config, system: &System) -> anyhow::Result<()> {
    let logger = Arc::new(ReplicationLogger::open(&config.db_path)?);
    let logger_clone = logger.clone();
    let path_clone = config.db_path.clone();
    let enable_bottomless = config.enable_bottomless_replication;
    let db_factory = Arc::new(move || {
        let db_path = path_clone.clone();
        let hook = ReplicationLoggerHook::new(logger.clone());
        async move { LibSqlDb::new(db_path, hook, enable_bottomless) }
    });
    let service = DbFactoryService::new(db_factory.clone());
    if let Some(ref addr) = config.rpc_server_addr {
        system.register(
            run_rpc_server(
                *addr,
                config.rpc_server_tls,
                config.rpc_server_cert.clone(),
                config.rpc_server_key.clone(),
                config.rpc_server_ca_cert.clone(),
                db_factory,
                logger_clone,
            ),
            "rpc server",
        );
    }

    run_service(service, config, system).await?;

    Ok(())
}

pub async fn run_server(config: Config) -> anyhow::Result<()> {
    tracing::trace!("Backend: {:?}", config.backend);

    #[cfg(feature = "mwal_backend")]
    {
        if config.backend == Backend::Mwal {
            std::env::set_var("MVSQLITE_DATA_PLANE", config.mwal_addr.as_ref().unwrap());
        }
        VWAL_METHODS
            .set(config.mwal_addr.as_ref().map(|_| {
                Arc::new(Mutex::new(
                    sqld_libsql_bindings::mwal::ffi::libsql_wal_methods::new(),
                ))
            }))
            .map_err(|_| anyhow::anyhow!("wal_methods initialized twice"))?;
    }

    if config.enable_bottomless_replication {
        bottomless::static_init::register_bottomless_methods();
    }

    let (local_tunnel_shutdown, _) = localtunnel_client::broadcast::channel(1);
    if config.create_local_http_tunnel {
        let tunnel = localtunnel_client::open_tunnel(
            Some("https://localtunnel.me"),
            None,
            config.http_addr.map(|a| a.ip().to_string()).as_deref(),
            config.http_addr.map(|a| a.port()).unwrap_or(8080),
            local_tunnel_shutdown.clone(),
            3,
            None,
        )
        .await?;
        println!("HTTP tunnel created: {tunnel}");
    }

    loop {
        if !config.db_path.exists() {
            std::fs::create_dir_all(&config.db_path)?;
        }
        let system = System::new();

        match config.writer_rpc_addr {
            Some(ref addr) => start_primary(&config, &system, addr).await?,
            None => start_replica(&config, &system).await?,
        }

        let reset = HARD_RESET.clone();
        let shutdown = SHUTDOWN.clone();
        loop {
            pin_mut!(system);
            tokio::select! {
                _ = reset.notified() => {
                    hard_reset(&config, system).await?;
                    break;
                },
                _ = shutdown.notified() => {
                    return Ok(())
                }
                res = &mut system => {
                    dbg!(&res);
                    return res;
                },
                else => return Ok(()),
            }
        }
    }
}
