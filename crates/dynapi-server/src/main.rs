mod shutdown;
mod startup;

use dynapi_server::{
    App, Monotonic,
    config::Config,
    telemetry::{
        StdoutSink, Stream, Telemetry,
        heartbeat::HeartbeatTask,
        shutdown::{ProcessDeadline, SHUTDOWN_TIMEOUT},
    },
    transport,
};
use shutdown::Signals;
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::time::Instant;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Cannot initialize server runtime");
    let mut process_deadline = None;
    let result = runtime.block_on(run(&mut process_deadline));
    // Never join stuck spawn_blocking work. The watchdog remains armed through
    // runtime destruction and process::exit (including C atexit handlers).
    runtime.shutdown_timeout(Duration::ZERO);
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    };
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
    let sink: Arc<dyn Telemetry> = stdout.clone();
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
    let sink: Arc<dyn Telemetry> = delivery
        .as_ref()
        .map(|delivery| delivery.clone() as Arc<dyn Telemetry>)
        .unwrap_or(sink);
    let telemetry_budget = SHUTDOWN_TIMEOUT;
    #[cfg(feature = "telemetry-delivery")]
    let telemetry_budget = delivery
        .as_ref()
        .map(|sink| sink.total_shutdown_budget(telemetry_budget))
        .unwrap_or(telemetry_budget);
    let mut deadline = None;
    let mut stop = |started: Instant, budget: Duration| {
        // This is the only process timer. Every cleanup step receives its
        // absolute deadline; no sink arms a second watchdog.
        *process_deadline = Some(stdout.process_deadline(started, budget, 1));
        started + budget
    };
    let mut app_owner = None;
    #[cfg(feature = "telemetry-delivery")]
    let mut startup_finished = false;
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let config = Config::from_env()?;
        let mut signals = Signals::new()?;
        let clock: Arc<dyn dynapi_policy::Clock> = Arc::new(Monotonic::default());
        #[cfg(feature = "harness")]
        let clock: Arc<dyn dynapi_policy::Clock> = if config.fixed_clock {
            Arc::new(dynapi_server::FixedClock)
        } else {
            clock
        };
        #[cfg(feature = "harness")]
        let sink: Arc<dyn Telemetry> =
            if std::env::var("DYNAPI_HARNESS_RECORD_HOOKS").as_deref() == Ok("1") {
                Arc::new(dynapi_server::telemetry::HarnessSink)
            } else {
                sink.clone()
            };
        let app = Arc::new(App::new(config, clock, sink.clone())?);
        app_owner = Some(app.clone());
        let waiting = Instant::now();
        let outcome = tokio::select! {
            biased;
            _ = signals.recv() => {
                deadline = Some(stop(Instant::now(), app.config.drain_timeout + app.config.finalization_timeout));
                return Ok(());
            },
            outcome = startup::wait_for_catalog(&app) => outcome,
        };
        #[cfg(feature = "harness")]
        let listener = if let Some(fd) = inherited_fd {
            use std::os::fd::FromRawFd;
            // The harness transfers ownership of an inherited listening socket.
            let socket = unsafe { std::net::TcpListener::from_raw_fd(fd) };
            socket.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(socket)?
        } else {
            tokio::net::TcpListener::bind((bind, port)).await?
        };
        #[cfg(not(feature = "harness"))]
        let listener = tokio::net::TcpListener::bind((bind, port)).await?;
        let bound = Instant::now();
        let ready = app.catalog_status().loaded_at.is_some();
        stdout.emit(
            Stream::Terminal,
            serde_json::json!({
                "event": "api_ready", "address": listener.local_addr()?.to_string(),
                "catalogReady": ready, "catalogWaitOutcome": outcome,
                "catalogWaitMs": waiting.elapsed().as_secs_f64() * 1000.,
                "bindToCatalogReadyMs": if ready { Some(0) } else { None },
            }),
        );
        let readiness = (!ready).then(|| {
            tokio::spawn(startup::report_catalog_ready(
                app.clone(),
                stdout.clone(),
                bound,
            ))
        });
        let heartbeat = HeartbeatTask::start(Arc::downgrade(&app), app.config.heartbeat_seconds);
        let mut server = transport::Server::new(listener, app.clone());
        #[cfg(feature = "telemetry-delivery")]
        { startup_finished = true; }
        let result = tokio::select! {
            biased;
            _ = signals.recv() => Ok(()),
            result = server.accept() => result,
        };
        let stopped = Instant::now();
        deadline = Some(stop(
            stopped,
            app.config.drain_timeout + app.config.finalization_timeout,
        ));
        server.stop_accepting();
        if let Some(readiness) = readiness {
            readiness.abort();
            let _ = readiness.await;
        }
        heartbeat.stop().await;
        stdout.emit(
            Stream::Terminal,
            serde_json::json!({
                "event": "api_shutdown", "phase": "request_drain",
            }),
        );
        let drain_deadline = stopped + app.config.drain_timeout;
        let (request_drained, preparation) = tokio::join!(
            server.drain_until(drain_deadline),
            dynapi_reader::shutdown_preparation_until(drain_deadline),
        );
        let drained = request_drained && !preparation.occupied;
        stdout.emit(
            Stream::Terminal,
            serde_json::json!({
                "event": "api_shutdown_drained", "drained": drained,
                "computeInUse": app.gate.snapshot().in_use,
                "requestDrained": request_drained,
                "s8Preparation": preparation,
            }),
        );
        #[cfg(feature = "harness")]
        if std::env::var("DYNAPI_HARNESS_STUCK_FINALIZATION").as_deref() == Ok("1") {
            // Exercises synchronous cleanup which cannot be cancelled by Tokio.
            loop {
                std::thread::park();
            }
        }
        result?;
        if !drained {
            return Err("Request drain deadline exceeded".into());
        }
        Ok(())
    }
    .await;
    // Startup failure has no request drain. Bound its capture/flush and runtime
    // teardown too, including a stalled native resolver.
    let deadline = deadline.unwrap_or_else(|| stop(Instant::now(), telemetry_budget));
    #[cfg(feature = "telemetry-delivery")]
    if result.is_err()
        && !startup_finished
        && let Some(delivery) = &delivery
    {
        delivery.capture_startup_error();
    }
    flush_telemetry(
        &stdout,
        #[cfg(feature = "telemetry-delivery")]
        delivery.as_deref(),
        deadline,
        telemetry_budget,
    )
    .await;
    if let Some(app) = app_owner {
        app.close_catalog();
    }
    result
}

/// Request cancellation records precede this flush; catalog close follows it.
/// Cap telemetry's normal bound by the remaining shared process deadline.
async fn flush_telemetry(
    stdout: &StdoutSink,
    #[cfg(feature = "telemetry-delivery")] delivery: Option<
        &dynapi_server::telemetry::delivery::DeliverySink,
    >,
    deadline: Instant,
    budget: Duration,
) {
    let deadline = deadline.min(Instant::now() + budget);
    let reserve = budget - SHUTDOWN_TIMEOUT;
    let drain_deadline = deadline - reserve;
    let network_shutdown = async {
        #[cfg(feature = "telemetry-delivery")]
        if let Some(delivery) = delivery
            && !delivery.shutdown_until(drain_deadline, deadline).await
        {
            eprintln!("Telemetry shutdown deadline exceeded; pending SDK events may be lost");
        }
    };
    tokio::join!(stdout.shutdown_until(drain_deadline), network_shutdown);
    stdout.report_loss();
}
