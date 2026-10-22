use dynapi_server::{
    App, Monotonic,
    config::Config,
    telemetry::{
        StdoutSink,
        shutdown::{ProcessDeadline, SHUTDOWN_TIMEOUT},
    },
    transport,
};
use std::{net::IpAddr, sync::Arc};

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Cannot initialize server runtime");
    let mut process_deadline = None;
    let result = runtime.block_on(run(&mut process_deadline));
    // Blocking DNS and compute work cannot be cancelled by dropping their
    // futures. Process exit ends any such work after bounded async cleanup.
    runtime.shutdown_timeout(std::time::Duration::ZERO);
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error:?}");
            1
        }
    };
    drop(process_deadline);
    std::process::exit(code);
}

async fn run(
    process_deadline: &mut Option<ProcessDeadline>,
) -> Result<(), Box<dyn std::error::Error>> {
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
    let stdout = Arc::new(StdoutSink::default());
    let sink: Arc<dyn dynapi_server::telemetry::Telemetry> = stdout.clone();
    #[cfg(feature = "telemetry-delivery")]
    let delivery = match dynapi_server::telemetry::delivery::DeliverySink::from_env_with_stdout(
        stdout.clone(),
    ) {
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
    let mut shutdown_started = None;
    let mut stop = |failed: bool| {
        let stopped = tokio::time::Instant::now();
        let budget = SHUTDOWN_TIMEOUT;
        #[cfg(feature = "telemetry-delivery")]
        let budget = delivery
            .as_ref()
            .map(|sink| sink.total_shutdown_budget(budget))
            .unwrap_or(budget);
        *process_deadline = Some(stdout.process_deadline(stopped, budget, i32::from(failed)));
        stopped
    };
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
        let (served, _stopped) = tokio::select! {
            result = transport::run(listener, app) => { let stopped = stop(result.is_err()); (result, stopped) },
            _ = tokio::signal::ctrl_c() => (Ok(()), stop(false)),
            _ = terminate() => (Ok(()), stop(false)),
        };
        shutdown_started = Some(_stopped);
        heartbeat.stop().await;
        served?;
        Ok(())
    }
    .await;
    let shutdown_started = shutdown_started.unwrap_or_else(|| stop(result.is_err()));
    let network_shutdown = async {
        #[cfg(feature = "telemetry-delivery")]
        if let Some(delivery) = delivery {
            if result.is_err() {
                use dynapi_server::telemetry::{ErrorKind, Stream, Telemetry, error_record};
                delivery.emit(
                    Stream::Error,
                    error_record(ErrorKind::Startup, serde_json::json!({})).unwrap(),
                );
            }
            if !delivery
                .shutdown_started_at(shutdown_started, SHUTDOWN_TIMEOUT)
                .await
            {
                eprintln!("Telemetry shutdown deadline exceeded; pending SDK events may be lost");
            }
        }
    };
    tokio::join!(
        stdout.shutdown_started_at(shutdown_started, SHUTDOWN_TIMEOUT),
        network_shutdown
    );
    stdout.report_loss();
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
