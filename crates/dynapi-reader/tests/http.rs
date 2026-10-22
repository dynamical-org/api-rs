//! Offline HTTP tests over synthetic inputs, with recipe-derived expectations.
use std::{
    io,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use dynapi_reader::{
    ErrorKind, HttpVirtualChunkContainer, ReadTrace, ReaderError, Selector, StoreDescriptor,
    TypedBuffer, read_selection_traced_with_containers,
};
use icechunk::{
    Repository,
    format::{
        ByteRange,
        manifest::{Checksum, SecondsSinceEpoch, VirtualChunkLocation, VirtualChunkRef},
    },
    store::Store,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::RwLock,
    task::{JoinHandle, JoinSet},
};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/http")
}

fn snapshot() -> String {
    let manifest: Value =
        serde_json::from_slice(include_bytes!("fixtures/http/manifest.json")).unwrap();
    manifest["snapshot_id"].as_str().unwrap().to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Access {
    method: String,
    path: String,
    range: Option<String>,
}

#[derive(Default)]
struct Observed {
    accesses: Vec<Access>,
    bytes_sent: usize,
    closed_writers: usize,
}

#[derive(Clone, Default)]
struct ServerOptions {
    status: Option<u16>,
    lz4_compressor: Option<u8>,
    redirect: Option<String>,
    payload: Option<Payload>,
    omit_checksums: bool,
    repository_fault: Option<(&'static str, RepositoryFault)>,
}

#[derive(Clone, Copy, Debug)]
enum RepositoryFault {
    Redirect,
    Oversized,
    WrongRange,
    Truncated,
    Unavailable,
}

#[derive(Clone, Copy)]
enum Payload {
    OversizedLength,
    OversizedUnframed,
    OversizedChunked,
    WrongRange,
    MissingLength,
    Truncated,
}

struct Server {
    base_url: String,
    observed: Arc<Mutex<Observed>>,
    task: JoinHandle<()>,
}

impl Server {
    async fn new(options: ServerOptions) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let observed = Arc::new(Mutex::new(Observed::default()));
        let state = observed.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let state = state.clone();
                        let options = options.clone();
                        connections.spawn(async move {
                            // Repository discovery may cancel optimistic requests.
                            if let Err(error) = respond(socket, state.clone(), options).await {
                                state.lock().unwrap().closed_writers += 1;
                                assert!(matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset), "{error}");
                            }
                        });
                    }
                    Some(result) = connections.join_next() => { result.unwrap(); }
                }
            }
        });
        Self {
            base_url,
            observed,
            task,
        }
    }

    fn descriptor(&self, local: bool) -> StoreDescriptor {
        if local {
            StoreDescriptor::Local(fixture().join("repository").to_str().unwrap().into())
        } else {
            StoreDescriptor::Http {
                url: format!("{}/repository/", self.base_url),
            }
        }
    }

    fn containers(&self) -> Vec<HttpVirtualChunkContainer> {
        vec![HttpVirtualChunkContainer {
            name: Some("allowed".into()),
            url_prefix: format!("{}/allowed/", self.base_url),
        }]
    }

    fn accesses(&self) -> Vec<Access> {
        self.observed.lock().unwrap().accesses.clone()
    }

    async fn read(
        &self,
        name: &str,
        local: bool,
        selection: Vec<Selector>,
        containers: &[HttpVirtualChunkContainer],
    ) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
        read_selection_traced_with_containers(
            &self.descriptor(local),
            &snapshot(),
            name,
            &selection,
            containers,
        )
        .await
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Aborting the accept task drops its JoinSet, which aborts all connections.
        self.task.abort();
    }
}

fn byte_range(header: &str, length: usize) -> Option<std::ops::Range<usize>> {
    let (first, last) = header.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = if first.is_empty() {
        (length.checked_sub(last.parse().ok()?)?, length)
    } else {
        let end = if last.is_empty() {
            length
        } else {
            last.parse::<usize>().ok()?.checked_add(1)?.min(length)
        };
        (first.parse().ok()?, end)
    };
    (start < end && end <= length).then_some(start..end)
}

async fn send_error(socket: &mut TcpStream, status: u16) -> io::Result<()> {
    socket
        .write_all(
            format!("HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
}

async fn respond(
    mut socket: TcpStream,
    state: Arc<Mutex<Observed>>,
    options: ServerOptions,
) -> io::Result<()> {
    let mut request = Vec::new();
    loop {
        let mut buffer = [0; 1024];
        let count = socket.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            break;
        }
        assert!(request.len() <= 16 * 1024, "oversized test request");
    }
    let request = std::str::from_utf8(&request).unwrap();
    let mut lines = request.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap();
    assert!(matches!(method, "HEAD" | "GET"));
    let path = first.next().unwrap().split('?').next().unwrap();
    let range = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("range"))
        .map(|(_, value)| value.trim());
    state.lock().unwrap().accesses.push(Access {
        method: method.into(),
        path: path.into(),
        range: range.map(str::to_owned),
    });
    if let Some((prefix, fault)) = options.repository_fault
        && path.starts_with(prefix)
    {
        return match fault {
            RepositoryFault::Redirect => socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: /denied/repo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await,
            RepositoryFault::Oversized => socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9999999999\r\nConnection: close\r\n\r\n").await,
            RepositoryFault::WrongRange => socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-16/30\r\nContent-Length: 16\r\nConnection: close\r\n\r\nXXXXXXXXXXXXXXXX").await,
            RepositoryFault::Truncated => socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\nX").await,
            RepositoryFault::Unavailable => send_error(&mut socket, 503).await,
        };
    }
    if let Some(status) = options.status {
        return send_error(&mut socket, status).await;
    }
    if path == "/allowed/values.bin" {
        if let Some(target) = &options.redirect {
            return socket.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;
        }
        if let Some(payload) = options.payload {
            return send_payload(&mut socket, &state, payload).await;
        }
    }
    let relative = path.trim_start_matches('/');
    if !Path::new(relative)
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return send_error(&mut socket, 403).await;
    }
    let target = match path {
        "/allowed/values.bin" | "/denied/values.bin" => fixture().join("values.bin"),
        "/allowed/lz4.bin" => fixture().join("lz4.bin"),
        _ => fixture().join(relative),
    };
    let mut data = match std::fs::read(target) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return send_error(&mut socket, 404).await;
        }
        Err(error) => return Err(error),
    };
    if path == "/allowed/lz4.bin"
        && let Some(compressor) = options.lz4_compressor
    {
        // Blosc flags are byte 2 of the frame, after seven padding bytes.
        data[9] = (data[9] & 0x1f) | (compressor << 5);
    }
    let span = match range {
        Some(header) => match byte_range(header, data.len()) {
            Some(span) => span,
            None => return send_error(&mut socket, 416).await,
        },
        None => 0..data.len(),
    };
    let status = if range.is_some() { 206 } else { 200 };
    let mut headers = format!(
        "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"synthetic-v1\"\r\nLast-Modified: Wed, 21 Oct 2026 00:00:00 GMT\r\nConnection: close\r\n",
        span.len(),
    );
    if range.is_some() {
        headers.push_str(&format!(
            "Content-Range: bytes {}-{}/{}\r\n",
            span.start,
            span.end - 1,
            data.len()
        ));
    }
    if options.omit_checksums {
        headers = headers
            .replace("ETag: \"synthetic-v1\"\r\n", "")
            .replace("Last-Modified: Wed, 21 Oct 2026 00:00:00 GMT\r\n", "");
    }
    headers.push_str("\r\n");
    socket.write_all(headers.as_bytes()).await?;
    if method == "GET" {
        socket.write_all(&data[span.clone()]).await?;
        state.lock().unwrap().bytes_sent += span.len();
    }
    Ok(())
}

async fn send_payload(
    socket: &mut TcpStream,
    state: &Arc<Mutex<Observed>>,
    payload: Payload,
) -> io::Result<()> {
    let length = 32 * 1024 * 1024;
    let framing = match payload {
        Payload::OversizedLength => format!("Content-Length: {length}\r\n"),
        Payload::OversizedChunked => "Transfer-Encoding: chunked\r\n".into(),
        Payload::OversizedUnframed | Payload::MissingLength => String::new(),
        _ => "Content-Length: 16\r\n".into(),
    };
    let range = if matches!(payload, Payload::WrongRange) {
        "8-23"
    } else {
        "7-22"
    };
    socket.write_all(format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {range}/30\r\n{framing}ETag: \"synthetic-v1\"\r\nLast-Modified: Wed, 21 Oct 2026 00:00:00 GMT\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    if matches!(payload, Payload::OversizedLength) {
        let mut buffer = [0; 1];
        if let Ok(result) = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            socket.read(&mut buffer),
        )
        .await
        {
            assert_eq!(result?, 0, "unexpected request bytes after headers");
            return Err(io::ErrorKind::ConnectionReset.into());
        }
    }
    if matches!(
        payload,
        Payload::MissingLength | Payload::WrongRange | Payload::Truncated
    ) {
        let end = if matches!(payload, Payload::Truncated) {
            11
        } else {
            23
        };
        socket
            .write_all(&include_bytes!("fixtures/http/values.bin")[7..end])
            .await?;
        return Ok(());
    }
    // Send only the requested bytes plus one excess byte, then wait for the
    // reader to close/reset. A collector that keeps reading gets the full 32 MiB.
    send_body(socket, state, payload, &[b'X'; 16]).await?;
    send_body(socket, state, payload, b"X").await?;
    let mut buffer = [0; 1];
    if let Ok(result) = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        socket.read(&mut buffer),
    )
    .await
    {
        assert_eq!(result?, 0, "unexpected request bytes after body");
        return Err(io::ErrorKind::ConnectionReset.into());
    }
    let mut sent = 17;
    while sent < length {
        let count = (length - sent).min(4096);
        send_body(socket, state, payload, &[b'X'; 4096][..count]).await?;
        sent += count;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    if matches!(payload, Payload::OversizedChunked) {
        socket.write_all(b"0\r\n\r\n").await?;
    }
    Ok(())
}

async fn send_body(
    socket: &mut TcpStream,
    state: &Arc<Mutex<Observed>>,
    payload: Payload,
    mut bytes: &[u8],
) -> io::Result<()> {
    if matches!(payload, Payload::OversizedChunked) {
        socket
            .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
            .await?;
    }
    while !bytes.is_empty() {
        let written = socket.write(bytes).await?;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        state.lock().unwrap().bytes_sent += written;
        bytes = &bytes[written..];
    }
    if matches!(payload, Payload::OversizedChunked) {
        socket.write_all(b"\r\n").await?;
    }
    Ok(())
}

async fn redirect_is_not_followed(other_origin: bool) {
    let destination = Server::new(ServerOptions::default()).await;
    let server = Server::new(ServerOptions {
        redirect: Some(if other_origin {
            format!("{}/denied/values.bin", destination.base_url)
        } else {
            "/denied/values.bin".into()
        }),
        ..Default::default()
    })
    .await;
    let result = server
        .read("allowed", true, selection(), &server.containers())
        .await;
    assert!(
        destination.accesses().is_empty(),
        "redirect destination was requested"
    );
    assert!(
        server
            .accesses()
            .iter()
            .all(|access| access.path == "/allowed/values.bin"),
        "redirect destination was requested"
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::Decode);
}

async fn oversized_body_is_stopped(payload: Payload, header_rejection: bool) {
    let server = Server::new(ServerOptions {
        payload: Some(payload),
        ..Default::default()
    })
    .await;
    let error = server
        .read("allowed", true, selection(), &server.containers())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode, "{error}");
    // Let the writer observe the FIN/reset; it must stop, not merely get aborted
    // when Server drops. The wire budget is the 16 requested bytes plus the
    // single byte needed to detect overflow; the collector stays capped at 16.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let sent = server.observed.lock().unwrap().bytes_sent;
    eprintln!(
        "16-byte HTTP range: server wrote {sent} bytes before close/reset (header rejection: {header_rejection})"
    );
    if header_rejection {
        assert_eq!(
            sent, 0,
            "inconsistent Content-Length was not rejected before the body"
        );
    }
    assert!(sent <= 17, "server sent {sent} bytes for a 16-byte range");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.observed.lock().unwrap().bytes_sent,
        sent,
        "writer did not stop after connection close/reset"
    );
    assert!(
        server.observed.lock().unwrap().closed_writers > 0,
        "writer did not observe close/reset"
    );
}

async fn missing_length_is_bounded() {
    let server = Server::new(ServerOptions {
        payload: Some(Payload::MissingLength),
        ..Default::default()
    })
    .await;
    let (buffer, _) = server
        .read("allowed", true, selection(), &server.containers())
        .await
        .unwrap();
    assert_values(&buffer);
}

async fn invalid_payload_response(payload: Payload) {
    let server = Server::new(ServerOptions {
        payload: Some(payload),
        ..Default::default()
    })
    .await;
    let error = server
        .read("allowed", true, selection(), &server.containers())
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode, "{error}");
}

fn selection() -> Vec<Selector> {
    vec![Selector::Indices(vec![3, 0, 1, 2, 0])]
}

fn assert_values(buffer: &TypedBuffer) {
    assert_eq!(buffer.dtype, "float32");
    assert_eq!(buffer.shape, [5]);
    // The recipe declares four IEEE values; selected order includes a duplicate.
    let values = [1.25_f32, -0.0, -3.5, 42.0];
    let expected: Vec<_> = [3, 0, 1, 2, 0]
        .into_iter()
        .flat_map(|index| values[index].to_bits().to_le_bytes())
        .collect();
    assert_eq!(buffer.bytes, expected);
}

async fn http_storage(trailing_slash: bool) {
    let server = Server::new(ServerOptions::default()).await;
    let descriptor = StoreDescriptor::Http {
        url: format!(
            "{}/repository{}",
            server.base_url,
            if trailing_slash { "/" } else { "" }
        ),
    };
    let (buffer, trace) = read_selection_traced_with_containers(
        &descriptor,
        &snapshot(),
        "stored",
        &selection(),
        &[],
    )
    .await
    .unwrap();
    assert_values(&buffer);
    assert_eq!(trace.decoded_bytes, 16);
    assert!(server.observed.lock().unwrap().bytes_sent > 16);
    assert!(
        server
            .accesses()
            .iter()
            .any(|access| access.path.contains("/chunks/"))
    );
    let (full, _) = read_selection_traced_with_containers(
        &descriptor,
        &snapshot(),
        "stored",
        &[Selector::Indices(vec![0, 1, 2, 3])],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(full.dtype, "float32");
    assert_eq!(full.shape, [4]);
    let expected: Vec<_> = [1.25_f32, -0.0, -3.5, 42.0]
        .into_iter()
        .flat_map(|value| value.to_bits().to_le_bytes())
        .collect();
    assert_eq!(full.bytes, expected);
}

async fn allowed_virtual_chunk(local: bool) {
    let server = Server::new(ServerOptions::default()).await;
    let (buffer, trace) = server
        .read("allowed", local, selection(), &server.containers())
        .await
        .unwrap();
    assert_values(&buffer);
    assert_eq!(trace.decoded_bytes, 16);
    let accesses: Vec<_> = server
        .accesses()
        .into_iter()
        .filter(|access| access.path.starts_with("/allowed/"))
        .collect();
    assert_eq!(
        accesses,
        vec![Access {
            method: "GET".into(),
            path: "/allowed/values.bin".into(),
            range: Some("bytes=7-22".into()),
        }]
    );
}

async fn undeclared_virtual_chunk(name: &str, declared: bool) {
    let server = Server::new(ServerOptions::default()).await;
    let containers = if declared {
        let mut containers = server.containers();
        containers.push(HttpVirtualChunkContainer {
            name: None,
            url_prefix: "https://example.invalid/chunks/".into(),
        });
        containers
    } else {
        vec![]
    };
    let error = server
        .read(name, true, selection(), &containers)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    assert!(
        error
            .message
            .to_lowercase()
            .contains("no virtual chunk container"),
        "{error}"
    );
    assert!(server.accesses().is_empty());
}

async fn http_storage_error(status: u16) {
    let server = Server::new(ServerOptions {
        status: Some(status),
        ..Default::default()
    })
    .await;
    let error = server
        .read("stored", false, selection(), &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Storage, "{error}");
    assert!(!server.accesses().is_empty());
}

#[tokio::test]
async fn repository_objects_cannot_bypass_bounded_transport() {
    for prefix in [
        "/repository/repo",
        "/repository/config.yaml",
        "/repository/snapshots/",
        "/repository/manifests/",
        "/repository/chunks/",
    ] {
        for fault in [
            RepositoryFault::Redirect,
            RepositoryFault::Oversized,
            RepositoryFault::Truncated,
            RepositoryFault::Unavailable,
        ] {
            let server = Server::new(ServerOptions {
                repository_fault: Some((prefix, fault)),
                ..Default::default()
            })
            .await;
            let result = server.read("stored", false, selection(), &[]).await;
            if prefix == "/repository/config.yaml" {
                // Icechunk probes this optimistically, but V2 keeps config in
                // repo and ignores errors from the unused V1 config file.
                assert_values(&result.unwrap().0);
            } else {
                assert!(result.is_err(), "{prefix}: {fault:?} was accepted");
            }
            assert!(
                server.accesses().iter().any(|a| a.path.starts_with(prefix)),
                "fault was not exercised: {prefix}"
            );
            assert!(
                server
                    .accesses()
                    .iter()
                    .all(|a| a.path.starts_with("/repository/")),
                "repository redirect escaped its prefix"
            );
        }
    }
    let server = Server::new(ServerOptions {
        repository_fault: Some(("/repository/chunks/", RepositoryFault::WrongRange)),
        ..Default::default()
    })
    .await;
    assert!(
        server
            .read("stored", false, selection(), &[])
            .await
            .is_err()
    );
}

async fn invalid_http_container(prefix: &str) {
    let server = Server::new(ServerOptions::default()).await;
    let containers = [HttpVirtualChunkContainer {
        name: None,
        url_prefix: prefix.into(),
    }];
    let error = server
        .read("allowed", true, selection(), &containers)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    assert!(server.accesses().is_empty());
}

async fn geographic_coordinates(name: &str, size: usize, origin: f64) {
    let server = Server::new(ServerOptions::default()).await;
    let (buffer, _) = server
        .read(
            name,
            false,
            vec![Selector::Indices((0..size as i64).collect())],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(buffer.dtype, "float64");
    assert_eq!(buffer.shape, [size]);
    let expected: Vec<_> = (0..size)
        .flat_map(|index| (origin + index as f64 / 4.0).to_bits().to_le_bytes())
        .collect();
    assert_eq!(buffer.bytes, expected);
}

async fn lz4_virtual_chunk() {
    let server = Server::new(ServerOptions::default()).await;
    let (buffer, trace) = server
        .read(
            "lz4_values",
            false,
            vec![Selector::Indices(vec![1023, 0, 100, 100])],
            &server.containers(),
        )
        .await
        .unwrap();
    assert_eq!(buffer.dtype, "float32");
    assert_eq!(buffer.shape, [4]);
    let expected: Vec<_> = [1023_u16, 0, 100, 100]
        .into_iter()
        .flat_map(|index| f32::from(index).to_bits().to_le_bytes())
        .collect();
    assert_eq!(buffer.bytes, expected);
    assert_eq!(trace.decoded_bytes, 4096);
    let (full, trace) = server
        .read(
            "lz4_values",
            false,
            vec![Selector::Indices((0..1024).collect())],
            &server.containers(),
        )
        .await
        .unwrap();
    assert_eq!(full.dtype, "float32");
    assert_eq!(full.shape, [1024]);
    let expected: Vec<_> = (0..1024_u16)
        .flat_map(|index| f32::from(index).to_bits().to_le_bytes())
        .collect();
    assert_eq!(full.bytes, expected);
    assert_eq!(trace.decoded_bytes, 4096);
    let payloads: Vec<_> = server
        .accesses()
        .into_iter()
        .filter(|access| access.path == "/allowed/lz4.bin")
        .collect();
    assert!(!payloads.is_empty());
    assert!(
        payloads
            .iter()
            .all(|access| access.range.as_deref() == Some("bytes=7-1152"))
    );
}

struct TempRepository(PathBuf);

impl TempRepository {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dynapi-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        let repository = Self(path);
        copy_tree(&fixture().join("repository"), &repository.0);
        repository
    }

    fn descriptor(&self) -> StoreDescriptor {
        StoreDescriptor::Local(self.0.to_str().unwrap().into())
    }
}

impl Drop for TempRepository {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            std::fs::create_dir(&target).unwrap();
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

async fn checksum_is_checked(checksum: Checksum, omit_headers: bool, succeeds: bool) {
    let server = Server::new(ServerOptions {
        omit_checksums: omit_headers,
        ..Default::default()
    })
    .await;
    let repository = TempRepository::new();
    let storage = icechunk::new_local_filesystem_storage(&repository.0)
        .await
        .unwrap();
    let repo = Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = Arc::new(RwLock::new(repo.writable_session("main").await.unwrap()));
    let store = Store::from_session(session.clone()).await;
    store
        .set_virtual_ref(
            "allowed/c/0",
            VirtualChunkRef {
                location: VirtualChunkLocation::from_url("vcc://allowed/values.bin").unwrap(),
                offset: 7,
                length: 16,
                checksum: Some(checksum),
            },
            false,
        )
        .await
        .unwrap();
    let snapshot = session
        .write()
        .await
        .commit("Synthetic checksum reference")
        .execute()
        .await
        .unwrap()
        .to_string();
    let result = read_selection_traced_with_containers(
        &repository.descriptor(),
        &snapshot,
        "allowed",
        &selection(),
        &server.containers(),
    )
    .await;
    if succeeds {
        assert_values(&result.unwrap().0);
    } else {
        assert_eq!(result.unwrap_err().kind, ErrorKind::Decode);
    }
}

async fn read_location(
    location: &str,
    containers: &[HttpVirtualChunkContainer],
) -> Result<(TypedBuffer, ReadTrace), ReaderError> {
    let repository = TempRepository::new();
    let storage = icechunk::new_local_filesystem_storage(&repository.0)
        .await
        .unwrap();
    let repo = Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = Arc::new(RwLock::new(repo.writable_session("main").await.unwrap()));
    let store = Store::from_session(session.clone()).await;
    store
        .set_virtual_ref(
            "allowed/c/0",
            VirtualChunkRef {
                location: VirtualChunkLocation::from_url(location).unwrap(),
                offset: 7,
                length: 16,
                checksum: None,
            },
            false,
        )
        .await
        .unwrap();
    let snapshot = session
        .write()
        .await
        .commit("Synthetic encoded reference")
        .execute()
        .await
        .unwrap()
        .to_string();
    read_selection_traced_with_containers(
        &repository.descriptor(),
        &snapshot,
        "allowed",
        &selection(),
        containers,
    )
    .await
}

async fn encoded_slash_read() {
    let server = Server::new(ServerOptions::default()).await;
    let result = read_location(
        &format!("{}/allowed%2Fvalues.bin", server.base_url),
        &server.containers(),
    )
    .await
    .unwrap();
    assert_values(&result.0);
}

async fn encoded_slash_redirect() {
    let destination = Server::new(ServerOptions::default()).await;
    let server = Server::new(ServerOptions {
        redirect: Some(format!("{}/denied/values.bin", destination.base_url)),
        ..Default::default()
    })
    .await;
    let result = read_location(
        &format!("{}/allowed%2Fvalues.bin", server.base_url),
        &server.containers(),
    )
    .await;
    assert!(
        destination.accesses().is_empty(),
        "encoded slash bypassed the bounded transport"
    );
    assert!(
        server
            .accesses()
            .iter()
            .all(|access| access.path == "/allowed/values.bin")
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::Decode);
}

async fn encoded_slash_body_limit() {
    let server = Server::new(ServerOptions {
        payload: Some(Payload::OversizedLength),
        ..Default::default()
    })
    .await;
    let error = read_location(
        &format!("{}/allowed%2Fvalues.bin", server.base_url),
        &server.containers(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        server.observed.lock().unwrap().bytes_sent,
        0,
        "encoded slash bypassed the header bound"
    );
    assert!(server.observed.lock().unwrap().closed_writers > 0);
}

async fn encoded_declaration_rejection() {
    let server = Server::new(ServerOptions::default()).await;
    let mut containers = server.containers();
    containers[0].url_prefix = format!("{}/allow%65d/", server.base_url);
    let error = server
        .read("allowed", true, selection(), &containers)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    assert!(server.accesses().is_empty());
}

async fn native_fallback_is_disabled() {
    let server = Server::new(ServerOptions::default()).await;
    let mut containers = server.containers();
    let authority = server
        .base_url
        .replace("http://", "http://fakeuser:fakepassword@");
    containers[0].url_prefix = format!("{authority}/allowed/");
    // Userinfo prevents matching the origin route, and the encoded slash prevents
    // matching the raw narrow route; the old native fallback would still match.
    let error = read_location(&format!("{authority}/allowed%2Fvalues.bin"), &containers)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode);
    assert!(
        server.accesses().is_empty(),
        "native fallback made a request"
    );
}

async fn changed_metadata(
    repository: &TempRepository,
    name: &str,
    section: &str,
    key: &str,
    value: Value,
) -> String {
    let storage = icechunk::new_local_filesystem_storage(&repository.0)
        .await
        .unwrap();
    let repo = Repository::open(None, storage, Default::default())
        .await
        .unwrap();
    let session = Arc::new(RwLock::new(repo.writable_session("main").await.unwrap()));
    let store = Store::from_session(session.clone()).await;
    let key_path = format!("{name}/zarr.json");
    let mut metadata: Value =
        serde_json::from_slice(&store.get(&key_path, &ByteRange::ALL).await.unwrap()).unwrap();
    let target = if section == "attributes" {
        &mut metadata["attributes"]
    } else {
        &mut metadata["codecs"][1]["configuration"]
    };
    target[key] = value;
    store
        .set(&key_path, serde_json::to_vec(&metadata).unwrap().into())
        .await
        .unwrap();
    session
        .write()
        .await
        .commit("Synthetic unsupported metadata")
        .execute()
        .await
        .unwrap()
        .to_string()
}

async fn lz4_metadata_is_exact(key: &str, value: Value) {
    let server = Server::new(ServerOptions::default()).await;
    let repository = TempRepository::new();
    let snapshot = changed_metadata(&repository, "lz4_values", "codec", key, value).await;
    let error = read_selection_traced_with_containers(
        &repository.descriptor(),
        &snapshot,
        "lz4_values",
        &selection(),
        &server.containers(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    assert!(server.accesses().is_empty());
}

async fn lz4_frame_must_match_metadata(compressor: u8) {
    let server = Server::new(ServerOptions {
        lz4_compressor: Some(compressor),
        ..Default::default()
    })
    .await;
    let error = server
        .read(
            "lz4_values",
            false,
            vec![Selector::Index(100)],
            &server.containers(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Decode, "{error}");
    assert!(error.message.contains("compressor"), "{error}");
}

async fn geographic_attributes_are_exact(name: &str, attribute: &str) {
    let server = Server::new(ServerOptions::default()).await;
    let repository = TempRepository::new();
    let snapshot = changed_metadata(
        &repository,
        name,
        "attributes",
        attribute,
        json!("unattested"),
    )
    .await;
    let error = read_selection_traced_with_containers(
        &repository.descriptor(),
        &snapshot,
        name,
        &[Selector::Index(0)],
        &server.containers(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error}");
    assert!(server.accesses().is_empty());
}

// Keep every acceptance case separately discoverable by ordinary cargo test.
macro_rules! cases {
    ($($name:ident => $body:expr;)*) => {$(
        #[tokio::test]
        async fn $name() { $body.await; }
    )*};
}

cases! {
    matching_etag_is_accepted => checksum_is_checked(Checksum::ETag(icechunk_types::ETag("synthetic-v1".into())), false, true);
    changed_etag_is_rejected => checksum_is_checked(Checksum::ETag(icechunk_types::ETag("different-v1".into())), false, false);
    missing_etag_is_rejected => checksum_is_checked(Checksum::ETag(icechunk_types::ETag("synthetic-v1".into())), true, false);
    matching_last_modified_is_accepted => checksum_is_checked(Checksum::LastModified(SecondsSinceEpoch(1_792_540_800)), false, true);
    changed_last_modified_is_rejected => checksum_is_checked(Checksum::LastModified(SecondsSinceEpoch(1)), false, false);
    missing_last_modified_is_rejected => checksum_is_checked(Checksum::LastModified(SecondsSinceEpoch(1_792_540_800)), true, false);
    encoded_slash_keeps_allowed_reads => encoded_slash_read();
    encoded_slash_cannot_follow_redirect => encoded_slash_redirect();
    encoded_slash_cannot_collect_oversized_body => encoded_slash_body_limit();
    encoded_declaration_is_not_fetched => encoded_declaration_rejection();
    native_http_fallback_is_disabled => native_fallback_is_disabled();
    redirect_same_origin_is_not_followed => redirect_is_not_followed(false);
    redirect_cross_origin_is_not_followed => redirect_is_not_followed(true);
    oversized_content_length_is_rejected => oversized_body_is_stopped(Payload::OversizedLength, true);
    oversized_unframed_body_is_stopped => oversized_body_is_stopped(Payload::OversizedUnframed, false);
    oversized_chunked_body_is_stopped => oversized_body_is_stopped(Payload::OversizedChunked, false);
    missing_content_length_is_bounded => missing_length_is_bounded();
    inconsistent_content_range_is_rejected => invalid_payload_response(Payload::WrongRange);
    truncated_body_is_rejected => invalid_payload_response(Payload::Truncated);
    http_storage_without_trailing_slash => http_storage(false);
    http_storage_with_trailing_slash => http_storage(true);
    allowed_virtual_chunk_http_repository => allowed_virtual_chunk(false);
    allowed_virtual_chunk_local_repository => allowed_virtual_chunk(true);
    undeclared_virtual_chunk_is_not_fetched => undeclared_virtual_chunk("allowed", false);
    undeclared_named_container_is_not_fetched => undeclared_virtual_chunk("denied", true);
    adjacent_prefix_is_not_fetched => undeclared_virtual_chunk("boundary", true);
    http_storage_403 => http_storage_error(403);
    http_storage_404 => http_storage_error(404);
    http_storage_429 => http_storage_error(429);
    http_storage_503 => http_storage_error(503);
    invalid_s3_container => invalid_http_container("s3://example/chunks/");
    invalid_file_container => invalid_http_container("file:///tmp/chunks/");
    invalid_container_without_trailing_slash => invalid_http_container("https://example.invalid/chunks");
    geographic_y => geographic_coordinates("y", 721, -90.0);
    geographic_x => geographic_coordinates("x", 1440, 0.0);
    weathernext_lz4_virtual_chunk => lz4_virtual_chunk();
    lz4_metadata_clevel => lz4_metadata_is_exact("clevel", json!(3));
    lz4_metadata_shuffle => lz4_metadata_is_exact("shuffle", json!("bitshuffle"));
    lz4_metadata_typesize => lz4_metadata_is_exact("typesize", json!(8));
    lz4_metadata_blocksize => lz4_metadata_is_exact("blocksize", json!(1024));
    lz4_frame_blosclz => lz4_frame_must_match_metadata(0);
    lz4_frame_snappy => lz4_frame_must_match_metadata(2);
    lz4_frame_zstd => lz4_frame_must_match_metadata(4);
    geographic_y_axis => geographic_attributes_are_exact("y", "axis");
    geographic_x_axis => geographic_attributes_are_exact("x", "axis");
    geographic_y_statistics => geographic_attributes_are_exact("y", "statistics_approximate");
    geographic_x_statistics => geographic_attributes_are_exact("x", "statistics_approximate");
}
