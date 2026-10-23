use super::*;
use icechunk::{
    config::HttpConfig, format::manifest::SecondsSinceEpoch, storage::ETag,
    virtual_chunks::HttpVirtualChunkFetcher,
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
    task::JoinHandle,
};

const SLOW: &[u8] = b"<Error><Code>SlowDown</Code></Error>";

#[test]
fn only_complete_bounded_error_documents_are_slow_down() {
    for bytes in [SLOW, b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>SlowDown</Code><Message>retry</Message></Error>", b" \n<Error><Code>SlowDown</Code></Error>\t"] {
        assert!(slow_down(bytes), "{bytes:?}");
    }
    for bytes in [
        b"<Error><Code>SlowDown</Code>".as_slice(),
        b"<Error><Code>SlowDown</Code></Wrong>",
        b"<Error><Code>SlowDown</Code><Code>SlowDown</Code></Error>",
        b"<Error><Message><Code>SlowDown</Code></Message></Error>",
        b"<Error><Code a=\"x\">SlowDown</Code></Error>",
        b"<Error><Code>SlowDown</Code><Message a=bad/></Error>",
        b"<Error><Code>SlowDown</Code><Message a=\"x\" a=\"y\"/></Error>",
        b"<Error><Code>SlowDown</Code><Message a=\"<\"/></Error>",
        b"<Error><Code><![CDATA[SlowDown]]></Code></Error>",
        b"<!DOCTYPE Error><Error><Code>SlowDown</Code></Error>",
        b"<Error><Code>Slow&#68;own</Code></Error>",
        b"<Error><Code>SlowDown</Code></Error><Error/>",
        b"<Error><Code>SlowDown</Code></Error>garbage",
        b"<Error><Code>SlowDown</Code></Error>\0",
        b"<?xml version=\"1.0\" encoding=\"UTF-16\"?><Error><Code>SlowDown</Code></Error>",
        b"<?xml version=\"1.0\" strange=\"x\"?><Error><Code>SlowDown</Code></Error>",
        b"<Error><Code>SlowDown</Code><Message><!-- a--b --></Message></Error>",
        b"<Error><Code>SlowDown</Code><1Invalid/></Error>",
        b"<Error><Code>SlowDown<Inner/></Code></Error>",
        b"<Error><Code>SlowDown</Code><Message>\xff</Message></Error>",
        b"<Error><Code>SlowDown</Code><Message/></Error><",
    ] {
        assert!(!slow_down(bytes), "{bytes:?}");
    }
    // Derive the padding from framing rather than depending on this literal.
    let framing = b"<Error><Code>SlowDown</Code><Message></Message></Error>";
    let exact = format!(
        "<Error><Code>SlowDown</Code><Message>{}</Message></Error>",
        "x".repeat(4096 - framing.len())
    );
    assert_eq!(exact.len(), 4096);
    assert!(slow_down(exact.as_bytes()));
    assert!(!slow_down(format!("{exact} ").as_bytes()));
    assert!(!slow_down(
        format!("<Error><Code>{}</Code></Error>", "x".repeat(65)).as_bytes()
    ));
    for (nested, expected) in [(7, true), (8, false)] {
        let xml = format!(
            "<Error><Code>SlowDown</Code>{}{}</Error>",
            "<M>".repeat(nested),
            "</M>".repeat(nested)
        );
        assert_eq!(slow_down(xml.as_bytes()), expected);
    }
    // Root + Code start/text/end + 250 empty children + root end + EOF =256.
    let events = format!("<Error><Code>SlowDown</Code>{}</Error>", "<M/>".repeat(250));
    assert!(slow_down(events.as_bytes()));
    assert!(!slow_down(
        events.replace("</Error>", "<M/></Error>").as_bytes()
    ));
}

#[test]
fn parser_scratch_is_bounded_with_borrowed_input_and_attribute_checks() {
    let attrs = (0..32).map(|i| format!(" a{i}=\"x\"")).collect::<String>();
    let xml = format!("<Error{attrs}><Code>SlowDown</Code></Error>");
    assert!(xml.len() < 4096);
    let framing = "<Error><Code>SlowDown</Code><Message></Message></Error>";
    let byte_limit = framing.replace(
        "</Message>",
        &format!("{}</Message>", "x".repeat(4096 - framing.len())),
    );
    let depth_limit = format!(
        "<Error><Code>SlowDown</Code>{}{}</Error>",
        "<M>".repeat(7),
        "</M>".repeat(7)
    );
    let event_limit = format!("<Error><Code>SlowDown</Code>{}</Error>", "<M/>".repeat(250));
    for (label, input, expected) in [
        ("attributes", xml, true),
        ("bytes", byte_limit.clone(), true),
        ("bytes+1", format!("{byte_limit} "), false),
        ("depth", depth_limit.clone(), true),
        (
            "depth+1",
            format!(
                "<Error><Code>SlowDown</Code>{}{}</Error>",
                "<M>".repeat(8),
                "</M>".repeat(8)
            ),
            false,
        ),
        ("events", event_limit.clone(), true),
        (
            "events+1",
            event_limit.replace("</Error>", "<M/></Error>"),
            false,
        ),
    ] {
        let measurement = crate::allocation_tests::Measurement::start();
        let accepted = slow_down(input.as_bytes());
        let counts = measurement.counts();
        drop(measurement);
        assert_eq!(accepted, expected, "{label}");
        assert_eq!(counts.live, 0, "{label}");
        assert!(!counts.foreign_deallocation, "{label}");
        assert!(counts.peak <= 8192, "{label}: {counts:?}");
        eprintln!(
            "parser scratch {label}: {counts:?}; fixed tag stack={} bytes",
            std::mem::size_of::<[Option<BytesStart<'_>>; 8]>()
        );
    }
}

#[derive(Clone)]
enum AfterBody {
    Close,
    Stall,
    Reset {
        attempts: Arc<super::super::transport::Attempts>,
        observed: u64,
    },
}

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
    length: usize,
    pause: Option<Arc<Notify>>,
    after: AfterBody,
}
impl Reply {
    fn new(status: u16, body: &[u8]) -> Self {
        Self {
            status,
            body: body.to_vec(),
            headers: vec![],
            length: body.len(),
            pause: None,
            after: AfterBody::Close,
        }
    }
    fn success() -> Self {
        let mut reply = Self::new(206, b"data");
        reply
            .headers
            .push(("Content-Range", "bytes 7-10/20".into()));
        reply
    }
}
struct Origin {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    notified: Arc<Notify>,
    task: JoinHandle<()>,
}
impl Origin {
    async fn new(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}/object", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let notified = Arc::new(Notify::new());
        let seen = requests.clone();
        let signal = notified.clone();
        let task = tokio::spawn(async move {
            let mut replies = VecDeque::from(replies);
            let mut stalled = Vec::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    if socket.read_exact(&mut byte).await.is_err() {
                        break;
                    }
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 16384);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                signal.notify_waiters();
                let reply = replies.pop_front().expect("unexpected physical request");
                if reply.status == 0 {
                    continue;
                }
                let mut head = format!(
                    "HTTP/1.1 {} Test\r\nConnection: close\r\nContent-Length: {}\r\n",
                    reply.status, reply.length
                );
                for (key, value) in reply.headers {
                    head.push_str(&format!("{key}: {value}\r\n"));
                }
                head.push_str("\r\n");
                if socket.write_all(head.as_bytes()).await.is_err() {
                    continue;
                }
                if let Some(pause) = reply.pause {
                    pause.notified().await;
                }
                let _ = socket.write_all(&reply.body).await;
                match reply.after {
                    AfterBody::Close => {}
                    AfterBody::Stall => stalled.push(socket),
                    AfterBody::Reset { attempts, observed } => {
                        // Wait for the client to observe the partial body, then
                        // reset. No timing assumption about when bytes arrived.
                        while attempts.counts().3 < observed {
                            tokio::task::yield_now().await;
                        }
                        #[allow(deprecated)]
                        socket.set_linger(Some(Duration::ZERO)).unwrap();
                    }
                }
            }
        });
        Self {
            url,
            requests,
            notified,
            task,
        }
    }
    async fn wait_for(&self, count: usize) {
        loop {
            let notified = self.notified.notified();
            if self.requests.lock().unwrap().len() >= count {
                return;
            }
            notified.await;
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn drained(transport: &ProviderTransport, sends: u64) {
    let (started, active, finished, _) = transport.inner.attempts.counts();
    assert_eq!((started, active, finished), (sends, 0, sends));
}

#[tokio::test]
async fn only_terminal_attempt_controls_classification() {
    let mut truncated = Reply::success();
    truncated.body = b"d".to_vec();
    let mut modified = Reply::success();
    modified.headers.push(("ETag", "\"changed\"".into()));
    let etag = Checksum::ETag(ETag("original".into()));
    for (final_reply, checksum, expected) in [
        (Reply::new(200, b"data"), None, ProviderFailure::Generic),
        (Reply::new(404, b"missing"), None, ProviderFailure::Generic),
        (
            Reply::new(503, b"unavailable"),
            None,
            ProviderFailure::Generic,
        ),
        (
            Reply::new(503, b"<Error><Code>SlowDown</Code>"),
            None,
            ProviderFailure::Generic,
        ),
        (truncated, None, ProviderFailure::Generic),
        (modified, Some(&etag), ProviderFailure::ObjectModified),
        (
            Reply::new(412, b"changed"),
            None,
            ProviderFailure::ObjectModified,
        ),
    ] {
        let origin = Origin::new(vec![Reply::new(503, SLOW), final_reply]).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        let error = transport
            .fetch(&origin.url, &(7..11), checksum)
            .await
            .unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(origin.count(), 2);
        drained(&transport, 2);
    }
    for first in [Reply::new(503, b"generic"), Reply::new(503, SLOW)] {
        let origin = Origin::new(vec![first, Reply::new(503, SLOW)]).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), None)
                .await
                .unwrap_err(),
            ProviderFailure::SlowDown
        );
        assert_eq!(origin.count(), 2);
        drained(&transport, 2);
    }
    let origin = Origin::new(vec![Reply::new(503, SLOW), Reply::success()]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    assert_eq!(
        transport
            .fetch(&origin.url, &(7..11), None)
            .await
            .unwrap()
            .data
            .as_ref(),
        b"data"
    );
    drained(&transport, 2);
}

#[tokio::test]
async fn provider_policy_is_explicit_and_legacy_condition_semantics_stay() {
    for provider in [Provider::Gcs, Provider::S3] {
        let origin = Origin::new(vec![Reply::new(503, SLOW), Reply::new(503, SLOW)]).await;
        let transport = ProviderTransport::new(provider).unwrap();
        let expected = if provider == Provider::S3 {
            ProviderFailure::SlowDown
        } else {
            ProviderFailure::Generic
        };
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), None)
                .await
                .unwrap_err(),
            expected
        );
        drained(&transport, 2);
    }
    let origin = Origin::new(vec![Reply::new(503, SLOW), Reply::new(503, SLOW)]).await;
    let legacy = crate::http::BoundedHttpFetcher::new(vec![origin.url.clone()]).unwrap();
    let error = legacy
        .fetch(&origin.url, &(7..11), None, &HttpConfig::default())
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("SlowDown"));
    assert_eq!(legacy.transport.attempts.counts().0, 2);
    assert_eq!(legacy.transport.attempts.counts().1, 0);
    let origin = Origin::new(vec![Reply::success()]).await;
    let legacy = crate::http::BoundedHttpFetcher::new(vec![origin.url.clone()]).unwrap();
    let checksum = Checksum::ETag(ETag("original".into()));
    // Legacy callback still returns absent validators for Icechunk to check.
    let result = legacy
        .fetch(
            &origin.url,
            &(7..11),
            Some(&checksum),
            &HttpConfig::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.etag, None);
    let request = origin.requests.lock().unwrap()[0].to_ascii_lowercase();
    assert!(request.contains("if-match: original\r\n"));
    assert!(request.contains("range: bytes=7-10\r\n"));
    let origin = Origin::new(vec![Reply::success()]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    assert_eq!(
        transport
            .fetch(&origin.url, &(7..11), Some(&checksum))
            .await
            .unwrap_err(),
        ProviderFailure::ObjectModified
    );
    assert!(
        origin.requests.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("if-match: \"original\"\r\n")
    );
    assert!(
        origin.requests.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("accept-encoding: identity\r\n")
    );
    drained(&transport, 1);
}

#[tokio::test]
async fn provider_checks_required_conditions_and_framing_after_fetch() {
    let etag = Checksum::ETag(ETag("opaque-3".into()));
    let modified = Checksum::LastModified(SecondsSinceEpoch(1_000));
    for (header, checksum, expected) in [
        (Some(("ETag", "\"opaque-3\"")), &etag, Ok(())),
        (
            Some(("ETag", "different")),
            &etag,
            Err(ProviderFailure::ObjectModified),
        ),
        (
            Some(("ETag", "\"é\"")),
            &etag,
            Err(ProviderFailure::ObjectModified),
        ),
        (None, &etag, Err(ProviderFailure::ObjectModified)),
        (None, &modified, Err(ProviderFailure::ObjectModified)),
        (
            Some(("Last-Modified", "Thu, 01 Jan 1970 00:16:40 GMT")),
            &modified,
            Ok(()),
        ),
        (
            Some(("Last-Modified", "Thu, 01 Jan 1970 00:16:41 GMT")),
            &modified,
            Err(ProviderFailure::ObjectModified),
        ),
        (
            Some(("Last-Modified", "invalid")),
            &modified,
            Err(ProviderFailure::ObjectModified),
        ),
        (
            Some(("Content-Encoding", "gzip")),
            &etag,
            Err(ProviderFailure::Generic),
        ),
    ] {
        let mut reply = Reply::success();
        if let Some((key, value)) = header {
            reply.headers.push((key, value.into()));
        }
        let origin = Origin::new(vec![reply]).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), Some(checksum))
                .await
                .map(|_| ()),
            expected
        );
        drained(&transport, 1);
    }
}

#[tokio::test]
async fn attempt_guard_lives_through_body_and_drains_on_cancel_or_expiry() {
    for (status, cancel) in [(206, true), (503, true), (503, false)] {
        let mut reply = if status == 206 {
            Reply::success()
        } else {
            Reply::new(503, SLOW)
        };
        reply.pause = Some(Arc::new(Notify::new()));
        let origin = Origin::new(vec![reply]).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        let worker = transport.clone();
        let url = origin.url.clone();
        let task = tokio::spawn(async move {
            worker
                .fetch_until(
                    &url,
                    &(7..11),
                    None,
                    Instant::now() + Duration::from_secs(2),
                )
                .await
        });
        origin.wait_for(1).await;
        assert_eq!(transport.inner.attempts.counts().1, 1);
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(
                task.await.unwrap().unwrap_err(),
                ProviderFailure::OperationTimeout
            );
        }
        drained(&transport, 1);
    }
}

#[tokio::test]
async fn expiry_during_backoff_overrides_prior_slow_down_without_new_send() {
    let origin = Origin::new(vec![Reply::new(503, SLOW)]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    let worker = transport.clone();
    let url = origin.url.clone();
    let task = tokio::spawn(async move {
        worker
            .fetch_until(
                &url,
                &(7..11),
                None,
                Instant::now() + Duration::from_secs(10),
            )
            .await
    });
    origin.wait_for(1).await;
    while transport.inner.attempts.counts().2 != 1 {
        tokio::task::yield_now().await;
    }
    // The first response/body is finished; the fixed first backoff is100ms.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(11)).await;
    assert_eq!(
        task.await.unwrap().unwrap_err(),
        ProviderFailure::OperationTimeout
    );
    assert_eq!(origin.count(), 1);
    drained(&transport, 1);
}

#[tokio::test]
async fn inner_retries_are_disabled_at_the_physical_http2_boundary() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let url = format!("http://{}/object", listener.local_addr().unwrap());
    let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = observed.clone();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted.unwrap();
                    let seen = seen.clone();
                    connections.spawn(async move {
                        let mut connection = h2::server::handshake(socket).await.unwrap();
                        while let Some(stream) = connection.accept().await {
                            let (_, mut response) = stream.unwrap();
                            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            response.send_reset(h2::Reason::REFUSED_STREAM);
                        }
                    });
                }
                Some(result) = connections.join_next() => { result.unwrap(); }
            }
        }
    });
    let mut transport = ProviderTransport::new(Provider::S3).unwrap();
    transport.inner.client = super::super::transport::BoundedRangeTransport::builder(true)
        .http2_prior_knowledge()
        .build()
        .unwrap();
    assert_eq!(
        transport.fetch(&url, &(7..11), None).await.unwrap_err(),
        ProviderFailure::Generic
    );
    assert_eq!(observed.load(std::sync::atomic::Ordering::Relaxed), 1);
    // A refused stream is not retryable by this object_store error adapter.
    // Removing retry-never makes reqwest send3 streams inside this one call.
    drained(&transport, 1);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn shared_client_keeps_logical_fetch_error_contexts_separate() {
    let slow = Origin::new(vec![Reply::new(503, SLOW), Reply::new(503, SLOW)]).await;
    let missing = Origin::new(vec![Reply::new(404, b"missing")]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    let (first, second) = tokio::join!(
        transport.fetch(&slow.url, &(7..11), None),
        transport.fetch(&missing.url, &(7..11), None)
    );
    assert_eq!(first.unwrap_err(), ProviderFailure::SlowDown);
    assert_eq!(second.unwrap_err(), ProviderFailure::Generic);
    drained(&transport, 3);
}

#[tokio::test]
async fn over_budget_or_interrupted_error_body_cannot_publish_slow_down() {
    let mut oversized = Reply::new(503, &vec![b'x'; 4097]);
    oversized.body[..SLOW.len()].copy_from_slice(SLOW);
    let mut interrupted = Reply::new(503, SLOW);
    interrupted.length += 1;
    for replies in [vec![oversized.clone(), oversized], vec![interrupted]] {
        let expected = replies.len() as u64;
        let origin = Origin::new(replies).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), None)
                .await
                .unwrap_err(),
            ProviderFailure::Generic
        );
        assert_eq!(origin.count() as u64, expected);
        drained(&transport, expected);
    }
}

#[tokio::test]
async fn connection_failure_retries_only_at_the_outer_boundary() {
    let origin = Origin::new(vec![Reply::new(0, b""), Reply::new(0, b"")]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    assert_eq!(
        transport
            .fetch(&origin.url, &(7..11), None)
            .await
            .unwrap_err(),
        ProviderFailure::Generic
    );
    assert_eq!(origin.count(), 2);
    drained(&transport, 2);
}

#[tokio::test]
async fn body_eof_releases_the_attempt_and_records_observed_bytes() {
    let release = Arc::new(Notify::new());
    let mut reply = Reply::success();
    reply.pause = Some(release.clone());
    let origin = Origin::new(vec![reply]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    let worker = transport.clone();
    let url = origin.url.clone();
    let task = tokio::spawn(async move { worker.fetch(&url, &(7..11), None).await });
    origin.wait_for(1).await;
    assert_eq!(transport.inner.attempts.counts().1, 1);
    release.notify_one();
    assert_eq!(task.await.unwrap().unwrap().data.as_ref(), b"data");
    drained(&transport, 1);
    assert_eq!(transport.inner.attempts.counts().3, 4);
}

#[tokio::test]
async fn unprocessed_http2_goaway_has_no_hidden_physical_retry() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let url = format!("http://{}/object", listener.local_addr().unwrap());
    let observed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = observed.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut preface = [0u8; 24];
            socket.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            // Empty SETTINGS; then GOAWAY(NO_ERROR,last_stream_id=0) after
            // receiving HEADERS. RFC9113§6.8 makes this request unprocessed.
            socket
                .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            loop {
                let mut header = [0u8; 9];
                socket.read_exact(&mut header).await.unwrap();
                let length = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
                assert!(length <= 16384);
                let mut payload = vec![0; length];
                socket.read_exact(&mut payload).await.unwrap();
                if header[3] == 4 && header[4] == 0 {
                    socket
                        .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                        .await
                        .unwrap();
                }
                if header[3] == 1 {
                    seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    socket
                        .write_all(&[0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                    break;
                }
            }
        }
    });
    let mut transport = ProviderTransport::new(Provider::S3).unwrap();
    transport.inner.client = super::super::transport::BoundedRangeTransport::builder(true)
        .http2_prior_knowledge()
        .build()
        .unwrap();
    assert_eq!(
        transport.fetch(&url, &(7..11), None).await.unwrap_err(),
        ProviderFailure::Generic
    );
    let sends = observed.load(std::sync::atomic::Ordering::Relaxed) as u64;
    assert!((1..=2).contains(&sends));
    let attempts = transport.inner.attempts.counts().0;
    assert!((sends..=2).contains(&attempts));
    drained(&transport, attempts);
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn rejected_provider_ranges_and_conditions_start_no_attempt() {
    let origin = Origin::new(vec![]).await;
    let transport = ProviderTransport::new(Provider::S3).unwrap();
    for range in [
        0..0,
        Range {
            start: u64::MAX,
            end: 0,
        },
        0..(8 * 1024 * 1024 + 1),
    ] {
        assert_eq!(
            transport
                .fetch(&origin.url, &range, None)
                .await
                .unwrap_err(),
            ProviderFailure::Generic
        );
    }
    for value in ["", "W/\"weak\"", "bad\r\ntag", "inner\"quote"] {
        let checksum = Checksum::ETag(ETag(value.into()));
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), Some(&checksum))
                .await
                .unwrap_err(),
            ProviderFailure::Generic
        );
    }
    assert_eq!(origin.count(), 0);
    drained(&transport, 0);
}

fn dense_attributes(count: usize) -> String {
    const FIRST: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const REST: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..count)
        .map(|i| {
            let name = if i < FIRST.len() {
                (FIRST[i] as char).to_string()
            } else {
                let index = i - FIRST.len();
                format!(
                    "{}{}",
                    FIRST[index % FIRST.len()] as char,
                    REST[index / FIRST.len()] as char
                )
            };
            format!(" {name}=\"\"")
        })
        .collect()
}

#[test]
fn attribute_limits_cover_the_densest_document_and_fixed_duplicate_scratch() {
    for count in [32, 33, 580, 654, 661, 685] {
        let xml = format!(
            "<Error{}><Code>SlowDown</Code></Error>",
            dense_attributes(count)
        );
        assert!(xml.len() <= 4096, "{count}: {}", xml.len());
        let measurement = crate::allocation_tests::Measurement::start();
        let accepted = slow_down(xml.as_bytes());
        let counts = measurement.counts();
        drop(measurement);
        eprintln!("attribute count={count}, bytes={}, {counts:?}", xml.len());
        assert!(counts.peak <= 8192, "{count}: {counts:?}");
        assert_eq!(counts.live, 0);
        assert!(!counts.foreign_deallocation);
        assert_eq!(accepted, count <= 32);
    }
    let attrs = dense_attributes(32);
    let long_attr_frame = "<Error =''><Code>SlowDown</Code></Error>";
    let long_attr = format!(
        "<Error {}=''><Code>SlowDown</Code></Error>",
        "a".repeat(4096 - long_attr_frame.len())
    );
    let decl_frame = "<?xml =''?><Error><Code>SlowDown</Code></Error>";
    let malformed_decl = format!(
        "<?xml {}=''?><Error><Code>SlowDown</Code></Error>",
        "a".repeat(4096 - decl_frame.len())
    );
    for (label, xml, expected) in [
        ("full-name", long_attr, true),
        ("full-decl-error", malformed_decl, false),
        (
            "many-tags",
            format!(
                "<Error><Code>SlowDown</Code>{}</Error>",
                format!("<M{attrs}/>").repeat(20)
            ),
            true,
        ),
        (
            "depth-and-attrs",
            format!(
                "<Error{attrs}><Code>SlowDown</Code>{}{}</Error>",
                format!("<M{attrs}>").repeat(7),
                "</M>".repeat(7)
            ),
            true,
        ),
        (
            "duplicate-last",
            format!(
                "<Error{} a=''><Code>SlowDown</Code></Error>",
                dense_attributes(31)
            ),
            false,
        ),
        (
            "child-over-limit",
            format!(
                "<Error><Code>SlowDown</Code><M{}/></Error>",
                dense_attributes(33)
            ),
            false,
        ),
    ] {
        assert!(xml.len() <= 4096, "{label}");
        let measurement = crate::allocation_tests::Measurement::start();
        let accepted = slow_down(xml.as_bytes());
        let counts = measurement.counts();
        drop(measurement);
        eprintln!(
            "attribute edge={label}, bytes={}, {counts:?}; duplicate stack={} bytes",
            xml.len(),
            std::mem::size_of::<[&[u8]; 32]>()
        );
        assert!(counts.peak <= 8192, "{label}: {counts:?}");
        assert_eq!(counts.live, 0);
        assert!(!counts.foreign_deallocation);
        assert_eq!(accepted, expected, "{label}");
    }
}

#[tokio::test]
async fn collection_stops_at_the_first_frame_past_4096_before_eof() {
    // The XML document is valid at4097bytes; the parser cap alone could mask
    // a larger collector. Withheld HTTP EOF additionally proves early drop.
    let document = [SLOW, &vec![b' '; 4097 - SLOW.len()]].concat();
    assert_eq!(document.len(), 4097);
    for withhold_eof in [false, true] {
        let mut reply = Reply::new(503, &document);
        if withhold_eof {
            reply.length += 1;
            reply.after = AfterBody::Stall;
        }
        let origin = Origin::new(vec![reply.clone(), reply]).await;
        let mut transport = ProviderTransport::new(Provider::S3).unwrap();
        transport.inner.client = super::super::transport::BoundedRangeTransport::builder(true)
            .read_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), None)
                .await
                .unwrap_err(),
            ProviderFailure::Generic
        );
        assert_eq!(origin.count(), 2, "EOF withheld={withhold_eof}");
        drained(&transport, 2);
        assert_eq!(transport.inner.attempts.counts().3, 2 * 4097);
    }
}

#[tokio::test]
async fn precondition_failed_status_is_independent_of_its_body() {
    for (body, length, stalled) in [
        (b"changed".to_vec(), 7, false),
        (b"short".to_vec(), 90, false),
        (vec![b'x'; 4097], 4097, false),
        (vec![], 90, true),
    ] {
        let mut reply = Reply::new(412, &body);
        reply.length = length;
        if stalled {
            reply.after = AfterBody::Stall;
        }
        let origin = Origin::new(vec![reply]).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), None)
                .await
                .unwrap_err(),
            ProviderFailure::ObjectModified
        );
        assert_eq!(origin.count(), 1);
        drained(&transport, 1);
        assert_eq!(transport.inner.attempts.counts().3, 0);
    }
}

fn ranged_reply(body: &[u8], remainder: bool) -> Reply {
    let mut reply = Reply::new(206, body);
    reply.length = if remainder { 3 } else { 4 };
    reply.headers = vec![
        (
            "Content-Range",
            if remainder {
                "bytes 8-10/20"
            } else {
                "bytes 7-10/20"
            }
            .into(),
        ),
        ("ETag", "\"e1\"".into()),
    ];
    reply
}

#[tokio::test]
async fn etag_body_retries_share_the_header_attempt_budget_and_terminal_state() {
    // Include success, repeated EOF, RST and stall; mixed header/body failures
    // must share the same one-retry budget. Queue a third reply to detect leaks.
    for legacy_mode in [false, true] {
        for case in [
            "eof-ok",
            "eof-eof",
            "rst-rst",
            "stall-stall",
            "slow-eof",
            "eof-slow",
        ] {
            let mut transport = ProviderTransport::new(Provider::S3).unwrap();
            transport.inner.client =
                super::super::transport::BoundedRangeTransport::builder(!legacy_mode)
                    .read_timeout(Duration::from_secs(1))
                    .timeout(Duration::from_secs(3))
                    .build()
                    .unwrap();
            let mut first = ranged_reply(b"d", false);
            let mut last = ranged_reply(if case == "eof-ok" { b"ata" } else { b"a" }, true);
            match case {
                "rst-rst" => {
                    first.after = AfterBody::Reset {
                        attempts: transport.inner.attempts.clone(),
                        observed: 1,
                    };
                    last.after = AfterBody::Reset {
                        attempts: transport.inner.attempts.clone(),
                        observed: 2,
                    };
                }
                "stall-stall" => {
                    first.after = AfterBody::Stall;
                    last.after = AfterBody::Stall;
                }
                "slow-eof" => {
                    first = Reply::new(503, SLOW);
                    last = ranged_reply(b"d", false);
                }
                "eof-slow" => {
                    last = Reply::new(503, SLOW);
                }
                _ => {}
            }
            let origin = Origin::new(vec![first, last, ranged_reply(b"ata", true)]).await;
            let checksum = Checksum::ETag(ETag("e1".into()));
            if legacy_mode {
                let mut legacy =
                    crate::http::BoundedHttpFetcher::new(vec![origin.url.clone()]).unwrap();
                legacy.transport = transport.inner.clone();
                let result = legacy
                    .fetch(
                        &origin.url,
                        &(7..11),
                        Some(&checksum),
                        &HttpConfig::default(),
                    )
                    .await;
                if case == "eof-ok" {
                    assert_eq!(result.unwrap().data.as_ref(), b"data");
                } else {
                    let text = result.unwrap_err().to_string();
                    assert!(!text.contains("SlowDown"));
                }
            } else {
                let result = transport
                    .fetch(&origin.url, &(7..11), Some(&checksum))
                    .await;
                let expected = match case {
                    "eof-ok" => Ok(()),
                    "eof-slow" => Err(ProviderFailure::SlowDown),
                    _ => Err(ProviderFailure::Generic),
                };
                assert_eq!(
                    result.map(|response| assert_eq!(response.data.as_ref(), b"data")),
                    expected,
                    "{case}"
                );
            }
            assert_eq!(origin.count(), 2, "{case}, legacy={legacy_mode}");
            drained(&transport, 2);
            if case != "slow-eof" {
                let request = origin.requests.lock().unwrap()[1].to_ascii_lowercase();
                assert!(
                    request.contains("range: bytes=8-10\r\n"),
                    "{case}: {request}"
                );
            }
        }
    }
}

#[tokio::test]
async fn invalid_required_validator_on_body_retry_keeps_its_own_class() {
    for last_modified in [false, true] {
        let mut first = ranged_reply(b"d", false);
        first
            .headers
            .push(("Last-Modified", "Thu, 01 Jan 1970 00:16:40 GMT".into()));
        let mut last = ranged_reply(b"ata", true);
        let checksum = if last_modified {
            last.headers.push(("Last-Modified", "invalid".into()));
            Checksum::LastModified(SecondsSinceEpoch(1000))
        } else {
            last.headers.retain(|(key, _)| *key != "ETag");
            last.headers.push(("ETag", "\"é\"".into()));
            Checksum::ETag(ETag("e1".into()))
        };
        let origin = Origin::new(vec![first, last, ranged_reply(b"ata", true)]).await;
        let transport = ProviderTransport::new(Provider::S3).unwrap();
        assert_eq!(
            transport
                .fetch(&origin.url, &(7..11), Some(&checksum))
                .await
                .unwrap_err(),
            ProviderFailure::ObjectModified
        );
        assert_eq!(origin.count(), 2);
        drained(&transport, 2);
    }
}

#[test]
fn xml_attributes_require_whitespace_between_quoted_values() {
    for body in [
        b"<Error a='x'b='y'><Code>SlowDown</Code></Error>".as_slice(),
        b"<?xml version='1.0'standalone='yes'?><Error><Code>SlowDown</Code></Error>",
        b"<Error><Other a=\"x\"b=\"y\"/><Code>SlowDown</Code></Error>",
    ] {
        assert!(!slow_down(body), "{}", String::from_utf8_lossy(body));
    }
    for whitespace in [" ", "\t", "\r", "\n"] {
        assert!(slow_down(
            format!("<Error a='x'{whitespace}b='y'><Code>SlowDown</Code></Error>").as_bytes()
        ));
    }
}
