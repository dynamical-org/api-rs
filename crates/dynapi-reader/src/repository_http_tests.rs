//! Synthetic wire responses; all requests stay on loopback.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
};

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}
impl Server {
    async fn new(response: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/repository", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, _) = accepted.unwrap();
                        let response = response.clone();
                        let observed = observed.clone();
                        connections.spawn(async move {
                            let mut request = Vec::new();
                            loop {
                                let mut buffer = [0; 1024];
                                let n = socket.read(&mut buffer).await.unwrap();
                                if n == 0 { return; }
                                request.extend_from_slice(&buffer[..n]);
                                if request.windows(4).any(|v| v == b"\r\n\r\n") { break; }
                                assert!(request.len() < 16384);
                            }
                            observed.lock().unwrap().push(String::from_utf8(request).unwrap());
                            let _ = socket.write_all(&response).await;
                        });
                    }
                    Some(result) = connections.join_next() => { result.unwrap(); }
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    fn storage(&self) -> BoundedRepositoryStorage {
        BoundedRepositoryStorage::new(&self.url).unwrap()
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response(status: u16, headers: &str, body: &[u8]) -> Vec<u8> {
    let mut result =
        format!("HTTP/1.1 {status} Fixture\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    result.extend_from_slice(body);
    result
}
fn fault(error: &StorageError) -> &ResponseError {
    match error.kind() {
        icechunk_storage::StorageErrorKind::ObjectStore(error) => error.downcast_ref().unwrap(),
        other => panic!("unexpected storage error {other:?}"),
    }
}

#[tokio::test]
async fn full_reads_all_entry_points_and_serialization() {
    let server = Server::new(response(
        200,
        "Content-Length: 4\r\nETag: \"fixture\"\r\n",
        b"data",
    ))
    .await;
    let storage: Box<dyn Storage> = Box::new(server.storage());
    let encoded = serde_json::to_string(&storage).unwrap();
    let storage: Box<dyn Storage> = serde_json::from_str(&encoded).unwrap();
    assert!(!storage.can_write().await.unwrap());
    let settings = Settings::default();
    for path in [
        "config.yaml",
        "repo",
        "snapshots/ABC",
        "manifests/ABC",
        "chunks/ABC",
    ] {
        let (mut reader, version) = storage.get_object(&settings, path, None).await.unwrap();
        let mut data = Vec::new();
        reader.read_to_end(&mut data).await.unwrap();
        assert_eq!(data, b"data");
        assert_eq!(version.etag(), Some(&"\"fixture\"".to_owned()));
    }
    let modified = storage
        .get_object_conditional(&settings, "repo", None)
        .await
        .unwrap();
    let GetModifiedResult::Modified { mut data, .. } = modified else {
        panic!("unconditional read must be modified")
    };
    let mut bytes = Vec::new();
    data.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, b"data");
    assert_eq!(server.count(), 6);
}

#[tokio::test]
async fn redirects_never_request_same_or_other_origin() {
    let destination = Server::new(response(200, "Content-Length: 0\r\n", b"")).await;
    for target in ["/outside".into(), format!("{}/outside", destination.url)] {
        let server = Server::new(response(
            302,
            &format!("Location: {target}\r\n"),
            b"discard this error body",
        ))
        .await;
        let error = server.storage().read("repo", None, None).await.unwrap_err();
        assert!(matches!(fault(&error), ResponseError::Redirect));
        assert_eq!(server.count(), 1);
        assert!(server.requests.lock().unwrap()[0].starts_with("GET /repository/repo "));
        assert_eq!(destination.count(), 0);
    }
}

#[tokio::test]
async fn ranges_require_exact_206_and_checked_endpoints() {
    for (status, content_range) in [
        (200, "bytes 7-22/30"),
        (206, "bytes 8-23/30"),
        (206, "bytes 7-22/22"),
        (206, "bytes 7-18446744073709551615/30"),
        (206, "bytes 7-22/*"),
        (206, "garbage"),
        (416, "bytes */30"),
    ] {
        let server = Server::new(response(
            status,
            &format!("Content-Range: {content_range}\r\nContent-Length: 16\r\n"),
            &[1; 16],
        ))
        .await;
        assert!(
            server
                .storage()
                .read("chunks/ABC", Some(&(7..23)), None)
                .await
                .is_err()
        );
    }
    for length in [Some(16), None] {
        let header = length
            .map(|n| format!("Content-Length: {n}\r\n"))
            .unwrap_or_default();
        let server = Server::new(response(
            206,
            &format!("Content-Range: bytes 7-22/30\r\n{header}"),
            &[1; 16],
        ))
        .await;
        let (data, _) = server
            .storage()
            .read("chunks/ABC", Some(&(7..23)), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(data.as_ref(), &[1; 16]);
        assert!(server.requests.lock().unwrap()[0].contains("range: bytes=7-22\r\n"));
    }
}

#[tokio::test]
async fn lengths_and_content_coding_cannot_change_range_meaning() {
    for (headers, body) in [
        ("Content-Length: 17\r\n", vec![1; 17]),
        ("Content-Length: 15\r\n", vec![1; 15]),
        ("Content-Length: 16\r\n", vec![1; 15]),
        ("", vec![1; 15]),
        ("", vec![1; 17]),
        (
            "Transfer-Encoding: chunked\r\n",
            b"11\r\n12345678901234567\r\n0\r\n\r\n".to_vec(),
        ),
        (
            "Content-Encoding: gzip\r\nContent-Length: 16\r\n",
            vec![1; 16],
        ),
    ] {
        let server = Server::new(response(
            206,
            &format!("Content-Range: bytes 7-22/30\r\n{headers}"),
            &body,
        ))
        .await;
        assert!(
            server
                .storage()
                .read("chunks/ABC", Some(&(7..23)), None)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn full_object_caps_apply_to_every_category_without_body_allocation() {
    for (path, cap) in [
        ("config.yaml", MAX_CONFIG_BYTES),
        ("repo", MAX_CONFIG_BYTES),
        ("snapshots/ABC", MAX_ASSET_BYTES),
        ("manifests/ABC", MAX_ASSET_BYTES),
        ("transactions/ABC", MAX_ASSET_BYTES),
        ("chunks/ABC", MAX_RANGE_BYTES),
    ] {
        let server = Server::new(response(
            200,
            &format!("Content-Length: {}\r\n", cap + 1),
            b"",
        ))
        .await;
        let error = server.storage().read(path, None, None).await.unwrap_err();
        assert!(
            matches!(fault(&error), ResponseError::Size(limit) if *limit == cap),
            "{path}: {error}"
        );
    }
}

#[tokio::test]
async fn full_chunked_and_unframed_bodies_stop_at_config_cap() {
    let data = vec![b'X'; MAX_CONFIG_BYTES as usize + 1];
    for chunked in [false, true] {
        let (header, body) = if chunked {
            let mut body = format!("{:x}\r\n", data.len()).into_bytes();
            body.extend_from_slice(&data);
            body.extend_from_slice(b"\r\n0\r\n\r\n");
            ("Transfer-Encoding: chunked\r\n", body)
        } else {
            ("", data.clone())
        };
        let server = Server::new(response(200, header, &body)).await;
        let error = server
            .storage()
            .read("config.yaml", None, None)
            .await
            .unwrap_err();
        assert!(matches!(
            fault(&error),
            ResponseError::Size(MAX_CONFIG_BYTES)
        ));
    }
    let server = Server::new(response(200, "", b"data")).await;
    assert_eq!(
        server
            .storage()
            .read("config.yaml", None, None)
            .await
            .unwrap()
            .unwrap()
            .0
            .as_ref(),
        b"data"
    );
}

#[tokio::test]
async fn invalid_requests_reject_before_io() {
    let server = Server::new(response(200, "", b"")).await;
    for range in [
        4..4,
        Range { start: 5, end: 4 },
        0..MAX_RANGE_BYTES + 1,
        0..u64::MAX,
    ] {
        assert!(
            server
                .storage()
                .get_object(&Settings::default(), "chunks/ABC", Some(&range))
                .await
                .is_err()
        );
    }
    for path in [
        "../repo",
        "/repo",
        "chunks/../repo",
        "chunks//ABC",
        "chunks/%2e%2e/repo",
        "chunks/a?b",
        "chunks/a\\b",
    ] {
        let error = server.storage().read(path, None, None).await.unwrap_err();
        assert!(matches!(fault(&error), ResponseError::Path));
    }
    for url in [
        format!("{}?token=fake", server.url),
        format!("{}#fragment", server.url),
        "http://fake:fake@127.0.0.1/repo".into(),
    ] {
        assert!(BoundedRepositoryStorage::new(&url).is_err());
    }
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn status_errors_are_typed_and_retries_are_bounded() {
    for (status, attempts) in [(403, 1), (404, 1), (429, 2), (503, 2)] {
        let server = Server::new(response(status, "Content-Length: 9999999999\r\n", b"")).await;
        let error = server.storage().read("repo", None, None).await.unwrap_err();
        if status == 404 {
            assert!(matches!(
                error.kind(),
                icechunk_storage::StorageErrorKind::ObjectNotFound
            ));
        } else {
            assert!(matches!(fault(&error), ResponseError::Status(s) if *s == status));
        }
        assert_eq!(server.count(), attempts);
    }
}

#[tokio::test]
async fn conditional_and_head_semantics() {
    let server = Server::new(response(304, "", b"")).await;
    let version = VersionInfo::from_etag_only("\"fixture\"".into());
    assert!(matches!(
        server
            .storage()
            .get_object_conditional(&Settings::default(), "repo", Some(&version))
            .await
            .unwrap(),
        GetModifiedResult::OnLatestVersion
    ));
    assert!(server.requests.lock().unwrap()[0].contains("if-none-match: \"fixture\"\r\n"));
    assert!(server.storage().read("repo", None, None).await.is_err());
    let server = Server::new(response(
        200,
        "Last-Modified: Wed, 21 Oct 2026 00:00:00 GMT\r\nContent-Length: 9999999999\r\n",
        b"",
    ))
    .await;
    assert_eq!(
        server
            .storage()
            .get_object_last_modified("repo", &Settings::default())
            .await
            .unwrap()
            .to_rfc3339(),
        "2026-10-21T00:00:00+00:00"
    );
    assert!(server.requests.lock().unwrap()[0].starts_with("HEAD "));
}
