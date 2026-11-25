use std::net::SocketAddr;

use axum::serve;
use gateway::{
    AppConfig, GatewayCommand, GatewayProfile,
    application::{self, AppState},
    transport::{data_plane_router, router},
};
use thiserror::Error;
use tokio::{sync::watch, task::JoinSet};

#[derive(Debug, Error)]
enum StartupError {
    #[error("configuration error: {0}")]
    Config(#[from] gateway::ConfigError),
    #[error("{0}")]
    Application(#[from] gateway::application::ApplicationError),
    #[error("worker failed: {0}")]
    Worker(#[from] gateway::jobs::WorkerError),
    #[error("failed to bind metrics listener at {address}: {source}")]
    MetricsBind {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("failed to bind API listener at {address}: {source}")]
    Bind {
        address: SocketAddr,
        source: std::io::Error,
    },
    #[error("API server failed: {0}")]
    Server(#[from] std::io::Error),
    #[error("{0}")]
    Listener(#[from] gateway::listeners::ListenerError),
    #[error("metrics listener stopped")]
    MetricsStopped,
}

#[tokio::main]
async fn main() -> Result<(), StartupError> {
    dotenvy::dotenv().ok();
    let command = GatewayCommand::parse(std::env::args_os().skip(1))?;
    // Before logging starts, so nothing but the report reaches stdout.
    if let GatewayCommand::Doctor { json, network } = command {
        let code = gateway::doctor::run(gateway::doctor::Options { json, network }).await;
        std::process::exit(code);
    }
    tracing_subscriber::fmt::init();

    let config = AppConfig::from_env_for(command.clone())?;

    if matches!(&command, GatewayCommand::Migrate) {
        application::migrate(&config).await?;
        return Ok(());
    }

    if let GatewayCommand::CatalogApply { organization, file } = command {
        application::apply_catalog(&config, &organization, &file).await?;
        return Ok(());
    }

    if let GatewayCommand::Invitation {
        action,
        organization,
        organization_name,
        role,
        valid_hours,
        id,
    } = &command
    {
        for line in application::invitation(
            &config,
            *action,
            organization,
            organization_name.as_deref(),
            role.as_deref(),
            *valid_hours,
            id.as_deref(),
        )
        .await?
        {
            println!("{line}");
        }
        return Ok(());
    }

    if let GatewayCommand::AgentCreate {
        organization,
        organization_name,
        name,
        role,
        token_name,
        expires_in_days,
    } = &command
    {
        for line in application::agent_create(
            &config,
            organization,
            organization_name.as_deref(),
            name,
            role.as_deref(),
            token_name.as_deref(),
            *expires_in_days,
        )
        .await?
        {
            println!("{line}");
        }
        return Ok(());
    }

    if let GatewayCommand::BootstrapOwner {
        organization,
        organization_name,
        issuer,
        subject,
        display_name,
    } = &command
    {
        for line in application::bootstrap_owner(
            &config,
            organization,
            organization_name.as_deref(),
            issuer,
            subject,
            display_name.as_deref(),
        )
        .await?
        {
            println!("{line}");
        }
        return Ok(());
    }

    let lines = match &command {
        GatewayCommand::SetupLink => Some(application::setup_link(&config).await?),
        GatewayCommand::UserResetPassword { email } => {
            Some(application::reset_password_link(&config, email).await?)
        }
        GatewayCommand::AllowPasswords { organization } => {
            Some(application::allow_passwords(&config, organization).await?)
        }
        GatewayCommand::AdminGrant {
            email,
            issuer,
            subject,
        } => Some(
            application::admin_grant(
                &config,
                email.as_deref(),
                issuer.as_deref(),
                subject.as_deref(),
            )
            .await?,
        ),
        _ => None,
    };
    if let Some(lines) = lines {
        for line in lines {
            println!("{line}");
        }
        return Ok(());
    }

    if let GatewayCommand::Config {
        action,
        organization,
        version,
    } = &command
    {
        for line in
            application::config_pin(&config, *action, organization, version.as_deref()).await?
        {
            println!("{line}");
        }
        return Ok(());
    }

    if let GatewayCommand::CloudPrincipal {
        action,
        organization,
        principal,
    } = &command
    {
        let principal = principal
            .as_ref()
            .map(|(kind, value)| (kind.as_str(), value.as_str()));
        for line in application::cloud_principal(&config, *action, organization, principal).await? {
            println!("{line}");
        }
        return Ok(());
    }

    let Some(address) = config.metrics_addr else {
        return run(&config).await;
    };
    // The metrics listener is bound first and supervised with the process:
    // if it stops, the process exits rather than run unobserved.
    let listener = gateway::net::bind_listener(address)
        .map_err(|source| StartupError::MetricsBind { address, source })?;
    tracing::info!(%address, "metrics listening");
    let metrics = serve(listener, gateway::metrics::router()).into_future();
    tokio::select! {
        result = run(&config) => result,
        result = metrics => {
            if let Err(error) = result {
                tracing::error!(%error, "metrics listener failed");
            }
            Err(StartupError::MetricsStopped)
        }
    }
}

/// Runs the configured profile until shutdown.
async fn run(config: &AppConfig) -> Result<(), StartupError> {
    if config.profile == GatewayProfile::Worker {
        let worker = application::connect_worker(config).await?;
        return run_worker(worker).await;
    }

    let state = application::connect(config).await?;
    if config.profile != GatewayProfile::DataPlane {
        application::announce_setup(&state).await;
    }
    let leases = state.budget_leases();
    let rate_limit_leases = state.rate_limit_leases();
    let result = match config.profile {
        GatewayProfile::AllInOne => {
            let worker = application::connect_worker(config).await?;
            run_all_in_one(config, state, worker).await
        }
        GatewayProfile::DataPlane => run_api(config, data_plane_router(state)).await,
        _ => run_api(config, router(state)).await,
    };
    // Requests have finished: what this process leased goes back now
    // rather than when the worker finds it expired.
    if let Some(leases) = leases {
        leases.return_all().await;
    }
    if let Some(leases) = rate_limit_leases {
        leases.return_all().await;
    }
    result
}

async fn run_worker(worker: gateway::jobs::Worker) -> Result<(), StartupError> {
    tokio::select! {
        result = gateway::jobs::run_worker(worker) => result.map_err(Into::into),
        _ = shutdown_signal() => Ok(()),
    }
}

async fn run_api(config: &AppConfig, routes: axum::Router) -> Result<(), StartupError> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });
    serve_api(config, routes, shutdown_rx).await
}

async fn bind_api(address: SocketAddr) -> Result<tokio::net::TcpListener, StartupError> {
    let listener = gateway::net::bind_listener(address)
        .map_err(|source| StartupError::Bind { address, source })?;
    tracing::info!(%address, "gateway API listening");
    Ok(listener)
}

/// Serves `routes` on the plain listener (HTTP/1.1 and h2c) and on the TLS
/// and QUIC listeners the configuration turns on, until shutdown. If one
/// fails, the others stop with it.
async fn serve_api(
    config: &AppConfig,
    routes: axum::Router,
    shutdown: watch::Receiver<bool>,
) -> Result<(), StartupError> {
    use gateway::listeners;

    let routes = listeners::with_alt_svc(routes, &config.listeners);
    let plain = bind_api(config.bind_addr).await?;
    let mut servers = JoinSet::<Result<(), StartupError>>::new();
    // The peer address feeds sign-in rate limits.
    let service = routes
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    let stop = shutdown.clone();
    servers.spawn(async move {
        serve(plain, service)
            .with_graceful_shutdown(wait_for_shutdown(stop))
            .await
            .map_err(StartupError::Server)
    });
    let tls = &config.listeners;
    if let (Some(cert_file), Some(key_file)) = (&tls.cert_file, &tls.key_file) {
        let certificate = listeners::CertificateFiles::load(cert_file, key_file)?;
        servers.spawn({
            let certificate = certificate.clone();
            let stop = shutdown.clone();
            async move {
                certificate.watch(stop).await;
                Ok(())
            }
        });
        if let Some(address) = tls.tls_addr {
            let listener = gateway::net::bind_listener(address)
                .map_err(|source| listeners::ListenerError::TlsBind { address, source })?;
            tracing::info!(%address, "gateway API listening with TLS (HTTP/2, HTTP/1.1)");
            let serving = listeners::serve_tls(
                listener,
                certificate.clone(),
                routes.clone(),
                shutdown.clone(),
            );
            servers.spawn(async move { serving.await.map_err(StartupError::Listener) });
        }
        if let Some(address) = tls.http3_addr {
            let endpoint = listeners::bind_quic(address, &certificate)?;
            tracing::info!(%address, "gateway API listening with QUIC (HTTP/3)");
            let serving = listeners::serve_quic(endpoint, routes, shutdown);
            servers.spawn(async move { serving.await.map_err(StartupError::Listener) });
        }
    }
    while let Some(finished) = servers.join_next().await {
        // Dropping the set stops the other listeners.
        finished.map_err(|error| StartupError::Server(std::io::Error::other(error)))??;
    }
    Ok(())
}

async fn run_all_in_one(
    config: &AppConfig,
    state: AppState,
    worker: gateway::jobs::Worker,
) -> Result<(), StartupError> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let api = serve_api(config, router(state), shutdown_rx.clone());
    let worker = worker_until_shutdown(worker, shutdown_rx);
    let signal = shutdown_signal();
    tokio::pin!(api);
    tokio::pin!(worker);
    tokio::pin!(signal);

    tokio::select! {
        result = &mut api => {
            let _ = shutdown_tx.send(true);
            let _ = worker.await;
            result
        }
        result = &mut worker => {
            let _ = shutdown_tx.send(true);
            let _ = api.await;
            result.map_err(StartupError::Worker)
        }
        _ = &mut signal => {
            let _ = shutdown_tx.send(true);
            let (api_result, worker_result) = tokio::join!(api, worker);
            if let Err(error) = worker_result {
                return Err(StartupError::Worker(error));
            }
            api_result
        }
    }
}

async fn worker_until_shutdown(
    worker: gateway::jobs::Worker,
    shutdown: watch::Receiver<bool>,
) -> Result<(), gateway::jobs::WorkerError> {
    tokio::select! {
        result = gateway::jobs::run_worker(worker) => result,
        _ = wait_for_shutdown(shutdown) => Ok(()),
    }
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ = shutdown.changed().await;
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
