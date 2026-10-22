mod shutdown;
mod startup;

use dynapi_server::{App, Monotonic, config::Config, telemetry::StdoutSink, transport};
use shutdown::{ProcessDeadline, Signals};
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
    let config = Config::from_env()?;
    let mut signals = Signals::new()?;
    let clock: Arc<dyn dynapi_policy::Clock> = Arc::new(Monotonic::default());
    #[cfg(feature = "harness")]
    let clock: Arc<dyn dynapi_policy::Clock> = if config.fixed_clock {
        Arc::new(dynapi_server::FixedClock)
    } else {
        clock
    };
    let sink: Arc<dyn dynapi_server::telemetry::Telemetry> = Arc::new(StdoutSink);
    #[cfg(feature = "harness")]
    let sink: Arc<dyn dynapi_server::telemetry::Telemetry> =
        if std::env::var("DYNAPI_HARNESS_RECORD_HOOKS").as_deref() == Ok("1") {
            Arc::new(dynapi_server::telemetry::HarnessSink)
        } else {
            sink
        };
    let app = Arc::new(App::new(config, clock, sink)?);
    let waiting = Instant::now();
    let outcome = tokio::select! {
        outcome = startup::wait_for_catalog(&app) => outcome,
        _ = signals.recv() => {
            *process_deadline = Some(ProcessDeadline::start(std::time::Instant::now(), app.config.drain_timeout + app.config.finalization_timeout));
            app.close_catalog();
            return Ok(());
        },
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
    println!(
        "{}",
        serde_json::json!({
            "event": "api_ready", "address": listener.local_addr()?.to_string(),
            "catalogReady": ready, "catalogWaitOutcome": outcome,
            "catalogWaitMs": waiting.elapsed().as_secs_f64() * 1000.,
            "bindToCatalogReadyMs": if ready { Some(0) } else { None },
        })
    );
    let readiness =
        (!ready).then(|| tokio::spawn(startup::report_catalog_ready(app.clone(), bound)));
    let mut server = transport::Server::new(listener, app.clone());
    let result = tokio::select! {
        biased;
        _ = signals.recv() => Ok(()),
        result = server.accept() => result,
    };
    let stopped = Instant::now();
    *process_deadline = Some(ProcessDeadline::start(
        stopped.into_std(),
        app.config.drain_timeout + app.config.finalization_timeout,
    ));
    server.stop_accepting();
    if let Some(readiness) = readiness {
        readiness.abort();
    }
    println!(
        "{}",
        serde_json::json!({"event": "api_shutdown", "phase": "request_drain"})
    );
    let drained = server.drain_until(stopped + app.config.drain_timeout).await;
    println!(
        "{}",
        serde_json::json!({
            "event": "api_shutdown_drained", "drained": drained,
            "computeInUse": app.gate.snapshot().in_use,
        })
    );
    // Telemetry flush plugs in here, using the shared finalization deadline:
    // stopped + drain_timeout + finalization_timeout. Do not start a watchdog
    // or grant a fresh process budget in any individual cleanup step.
    #[cfg(feature = "harness")]
    if std::env::var("DYNAPI_HARNESS_STUCK_FINALIZATION").as_deref() == Ok("1") {
        // Exercises synchronous cleanup which cannot be cancelled by Tokio.
        loop {
            std::thread::park();
        }
    }
    app.close_catalog();
    result?;
    if !drained {
        return Err("Request drain deadline exceeded".into());
    }
    Ok(())
}
