#![cfg(all(feature = "harness", target_os = "linux"))]

use std::{
    io::{Read, Write},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn full_undrained_stdout_does_not_block_http_and_counts_loss_at_bounded_shutdown() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    use std::os::fd::AsRawFd;
    let listener_fd = listener.as_raw_fd();
    let mut fds = [-1; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    let mut read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    assert_eq!(
        unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) },
        4096
    );
    write.write_all(&[b'x'; 4096]).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dynapi-server"));
    command
        .env_clear()
        .env("DYNAMICAL_API_HEARTBEAT_SECONDS", "0")
        .env("DYNAMICAL_API_STAC_URL", "http://127.0.0.1:1/catalog.json")
        .env("TOKIO_WORKER_THREADS", "2")
        .args(["--fd", &listener_fd.to_string(), "--port", "0"])
        .stdout(Stdio::from(write))
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(listener_fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut process = Process(command.spawn().unwrap());
    drop(command);
    drop(listener);
    // Every missing API route emits one unsampled terminal failure. More than
    // 256 queued records plus one blocked write forces a counted overflow.
    const REQUESTS: u64 = 300;
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..REQUESTS {
            let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
            socket.write_all(b"GET /v1/example-missing HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n").await.unwrap();
            let mut reply = Vec::new();
            socket.read_to_end(&mut reply).await.unwrap();
            assert!(reply.starts_with(b"HTTP/1.1 404"), "{reply:?}");
        }
    })
    .await
    .expect("HTTP responses must complete while stdout stays full and undrained");
    let started = Instant::now();
    assert_eq!(
        unsafe { libc::kill(process.0.id() as i32, libc::SIGTERM) },
        0
    );
    while process.0.try_wait().unwrap().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(12),
            "shutdown exceeded the exit bound"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(process.0.wait().unwrap().success());
    assert!(
        started.elapsed() <= Duration::from_secs(11),
        "shutdown took {:?}",
        started.elapsed()
    );
    let mut stderr = String::new();
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(
        stderr.contains(&format!(
            "Stdout telemetry lost records: dropped={REQUESTS}, failed=0"
        )),
        "{stderr}"
    );
    // The test never drained stdout before process exit, including shutdown.
    let mut output = Vec::new();
    read.read_to_end(&mut output).unwrap();
    assert_eq!(output, vec![b'x'; 4096]);
}
