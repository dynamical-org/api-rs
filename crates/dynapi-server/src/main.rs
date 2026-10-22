use dynapi_server::{App, Monotonic, config::Config, telemetry::StdoutSink, transport};
use std::{net::IpAddr, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bind: IpAddr = "127.0.0.1".parse()?;
    let mut port: u16 = 8000;
    #[cfg(feature = "harness")]
    let mut inherited_fd: Option<i32> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                bind = args
                    .next()
                    .ok_or("--bind requires an IP address")?
                    .parse()?
            }
            "--port" => port = args.next().ok_or("--port requires a port")?.parse()?,
            #[cfg(feature = "harness")]
            "--fd" => inherited_fd = Some(args.next().ok_or("--fd requires an integer")?.parse()?),
            "--help" => {
                println!("dynapi-server [--port PORT] [--bind IP]");
                return Ok(());
            }
            _ => return Err(format!("Unknown argument {arg}").into()),
        }
    }
    let sink: Arc<dyn dynapi_server::telemetry::Telemetry> = Arc::new(StdoutSink);
    #[cfg(feature = "telemetry-delivery")]
    let delivery = match dynapi_server::telemetry::delivery::DeliverySink::from_env() {
        Ok(delivery) => Some(Arc::new(delivery)),
        Err(message) => {
            eprintln!("Telemetry disabled: {message}");
            None
        }
    };
    #[cfg(feature = "telemetry-delivery")]
    let sink: Arc<dyn dynapi_server::telemetry::Telemetry> = delivery
        .as_ref()
        .map(|delivery| delivery.clone() as Arc<dyn dynapi_server::telemetry::Telemetry>)
        .unwrap_or(sink);
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let config = Config::from_env()?;
        let clock: Arc<dyn dynapi_policy::Clock> = Arc::new(Monotonic::default());
        #[cfg(feature = "harness")]
        let clock: Arc<dyn dynapi_policy::Clock> = if config.fixed_clock {
            Arc::new(dynapi_server::FixedClock)
        } else {
            clock
        };
        #[cfg(feature = "harness")]
        let sink: Arc<dyn dynapi_server::telemetry::Telemetry> =
            if std::env::var("DYNAPI_HARNESS_RECORD_HOOKS").as_deref() == Ok("1") {
                Arc::new(dynapi_server::telemetry::HarnessSink)
            } else {
                sink
            };
        let app = Arc::new(App::new(config, clock, sink.clone())?);
        app.warm_catalog();
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        #[cfg(feature = "harness")]
        let listener = if let Some(fd) = inherited_fd {
            use std::os::fd::FromRawFd;
            // The harness transfers ownership of an inherited listening socket.
            let socket = unsafe { std::net::TcpListener::from_raw_fd(fd) };
            socket.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(socket)?
        } else {
            listener
        };
        let heartbeat = dynapi_server::telemetry::heartbeat::HeartbeatTask::start(
            Arc::downgrade(&app),
            app.config.heartbeat_seconds,
        );
        let served = tokio::select! {
            result = transport::run(listener, app) => result,
            _ = tokio::signal::ctrl_c() => Ok(()),
            _ = terminate() => Ok(()),
        };
        heartbeat.stop().await;
        served?;
        Ok(())
    }
    .await;
    #[cfg(feature = "telemetry-delivery")]
    if let Some(delivery) = delivery {
        if result.is_err() {
            use dynapi_server::telemetry::{ErrorKind, Stream, Telemetry, error_record};
            delivery.emit(
                Stream::Error,
                error_record(ErrorKind::Startup, serde_json::json!({})).unwrap(),
            );
        }
        delivery
            .shutdown(dynapi_server::telemetry::delivery::SHUTDOWN_TIMEOUT)
            .await;
    }
    result
}
#[cfg(unix)]
async fn terminate() {
    if let Ok(mut signal) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        signal.recv().await;
    }
}
#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}
