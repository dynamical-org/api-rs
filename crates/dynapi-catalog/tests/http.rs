#![cfg(feature = "native")]
use dynapi_catalog::{native::StacCatalog, refresh::CatalogConfig};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

async fn server(mode: &'static str) -> (String, JoinHandle<()>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let calls = calls.clone();
            tokio::spawn(async move {
                let mut request = vec![0; 8192];
                let n = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_ascii_lowercase();
                assert!(request.contains("accept-encoding: identity\r\n"));
                assert!(request.contains("user-agent: dynamical-api/0.1.0\r\n"));
                calls.fetch_add(1, Ordering::SeqCst);
                let body = if request.starts_with("get /child.json ") {
                    br#"{"id":"example","assets":{"icechunk":{"href":"https://data.dynamical.org/example/v1/"}}}"#.as_slice()
                } else {
                    br#"{"links":[{"rel":"child","href":"child.json"}]}"#.as_slice()
                };
                let response=match mode {
                    "redirect" if request.starts_with("get /root.json ") => "HTTP/1.1 302 Found\r\nLocation: /catalog.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_bytes().to_vec(),
                    "redirect_loop" => "HTTP/1.1 302 Found\r\nLocation: /root.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_bytes().to_vec(),
                    "redirect_invalid_path" => "HTTP/1.1 302 Found\r\nLocation: http:target.json\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_bytes().to_vec(),
                    "redirect_invalid_ascii" => "HTTP/1.1 302 Found\r\nLocation: /a\tb\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_bytes().to_vec(),
                    "compressed" => "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 3\r\nConnection: close\r\n\r\nzip".as_bytes().to_vec(),
                    "status404" => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_bytes().to_vec(),
                    "trickle" => {
                        if stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.is_err() {return;}
                        for _ in 0..100 {
                            if stream.write_all(b"1\r\n \r\n").await.is_err() {return;}
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                        return;
                    }
                    "oversized" => {
                        if stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.is_err() {return;}
                        let chunk=vec![b' ';64*1024];
                        for _ in 0..257 {
                            if stream.write_all(b"10000\r\n").await.is_err() || stream.write_all(&chunk).await.is_err() || stream.write_all(b"\r\n").await.is_err() {return;}
                        }
                        let _=stream.write_all(b"0\r\n\r\n").await;
                        return;
                    }
                    _ => {let mut response=format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).into_bytes();response.extend_from_slice(body);response}
                };
                let _ = stream.write_all(&response).await;
            });
        }
    });
    (format!("http://{address}/root.json"), task, count)
}
#[tokio::test]
async fn native_http_headers_relative_links_and_redirects() {
    for mode in ["plain", "redirect"] {
        let (url, task, calls) = server(mode).await;
        let catalog = StacCatalog::new(url, CatalogConfig::default()).unwrap();
        assert_eq!(catalog.list_collections().await.unwrap()[0].id, "example");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            if mode == "redirect" { 3 } else { 2 }
        );
        catalog.close();
        task.abort();
    }
}
#[tokio::test]
async fn native_http_redirect_limit_matches_httpx_without_retry() {
    let (url, task, calls) = server("redirect_loop").await;
    let catalog = StacCatalog::new(url, CatalogConfig::default()).unwrap();
    assert_eq!(
        catalog.list_collections().await.unwrap_err().to_string(),
        "STAC catalog refresh failed (TooManyRedirects)"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 21);
    catalog.close();
    task.abort();
}
#[tokio::test]
async fn malformed_redirects_follow_python_exception_and_retry_boundaries() {
    for (mode, class, expected_calls) in [
        ("redirect_invalid_path", "InvalidURL", 1),
        ("redirect_invalid_ascii", "RemoteProtocolError", 3),
    ] {
        let (url, task, calls) = server(mode).await;
        let catalog = StacCatalog::new(url, CatalogConfig::default()).unwrap();
        assert_eq!(
            catalog.list_collections().await.unwrap_err().to_string(),
            format!("STAC catalog refresh failed ({class})")
        );
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls, "{mode}");
        catalog.close();
        task.abort();
    }
}
#[tokio::test]
async fn native_http_rejects_compression_oversize_and_permanent_status_without_retry() {
    for (mode, class) in [
        ("compressed", "CatalogError"),
        ("oversized", "CatalogError"),
        ("status404", "HTTPStatusError"),
    ] {
        let (url, task, calls) = server(mode).await;
        let catalog = StacCatalog::new(url, CatalogConfig::default()).unwrap();
        assert_eq!(
            catalog.list_collections().await.unwrap_err().to_string(),
            format!("STAC catalog refresh failed ({class})"),
            "{mode}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{mode}");
        catalog.close();
        task.abort();
    }
}
#[tokio::test]
async fn native_http_trickling_peer_is_bounded_by_whole_refresh_deadline() {
    let (url, task, _) = server("trickle").await;
    let catalog = StacCatalog::new(
        url,
        CatalogConfig {
            load_deadline_seconds: 0.06,
            ..CatalogConfig::default()
        },
    )
    .unwrap();
    let started = std::time::Instant::now();
    assert!(catalog.list_collections().await.is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while catalog.status().outstanding_workers > 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(catalog.status().loaded_at.is_none());
    catalog.close();
    task.abort();
}
/// Serve `root` and record request targets. The child is served for any target
/// under `/child.json`, so only the recorded target shows which URL was fetched.
async fn recording_server(
    root: String,
) -> (String, JoinHandle<()>, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let targets = Arc::new(std::sync::Mutex::new(vec![]));
    let seen = targets.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (seen, root) = (seen.clone(), root.clone());
            tokio::spawn(async move {
                let mut request = vec![0; 8192];
                let n = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..n]).to_string();
                let target = request.split(' ').nth(1).unwrap().to_string();
                seen.lock().unwrap().push(target.clone());
                let body = if target.starts_with("/child.json") {
                    r#"{"id":"example","assets":{"icechunk":{"href":"https://data.dynamical.org/example/v1/"}}}"#.to_string()
                } else {
                    root
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{address}/dir/root.json"), task, targets)
}
#[tokio::test]
async fn native_http_requests_python_urljoin_targets_or_fails_closed() {
    for (href, expected) in [
        ("../child.json;p?x=1", Ok("/child.json;p?x=1")),
        ("sub/../../child.json", Ok("/child.json")),
        ("../child.json ", Ok("/child.json%20")),
        ("../child.json?x= ", Ok("/child.json?x=%20")),
        ("../child.json\u{1c}", Err("InvalidURL")),
        ("../child.json?x=\u{7f}", Err("InvalidURL")),
        ("../child.json#\u{0}", Err("InvalidURL")),
        ("../child.json?x='", Err("InvalidURL")),
        ("../child.json'", Ok("/child.json'")),
        ("../a/%2e%2e/child.json", Err("InvalidURL")),
        ("..\\\\other.example.test\\child.json", Err("InvalidURL")),
    ] {
        let root = serde_json::json!({"links":[{"rel":"child","href":href}]}).to_string();
        let (url, task, targets) = recording_server(root).await;
        let catalog = StacCatalog::new(url, CatalogConfig::default()).unwrap();
        let result = catalog.list_collections().await;
        let targets = targets.lock().unwrap().clone();
        match expected {
            Ok(target) => {
                assert_eq!(result.unwrap()[0].id, "example", "{href}");
                assert_eq!(targets, ["/dir/root.json", target], "{href}");
            }
            Err(class) => {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    format!("STAC catalog refresh failed ({class})"),
                    "{href}"
                );
                assert_eq!(targets, ["/dir/root.json"], "{href}");
            }
        }
        catalog.close();
        task.abort();
    }
}

#[tokio::test]
async fn invalid_root_url_aborts_without_an_http_request() {
    for suffix in ["\u{0}", "?q=\u{1c}", "#\u{7f}"] {
        let (url, task, targets) = recording_server("{}".into()).await;
        let catalog = StacCatalog::new(format!("{url}{suffix}"), CatalogConfig::default()).unwrap();
        assert_eq!(
            catalog.list_collections().await.unwrap_err().to_string(),
            "STAC catalog refresh failed (InvalidURL)"
        );
        assert!(targets.lock().unwrap().is_empty());
        assert_eq!(catalog.status().consecutive_failures, 1);
        catalog.close();
        task.abort();
    }
}

#[tokio::test]
async fn redirect_targets_obey_explicit_request_target_policy() {
    // Explicit request-target policy: spaces are percent-encoded; rewrites of
    // encoded dots, backslashes and query apostrophes are rejected before GET.
    for (location, expected) in [
        ("/child name.json", Ok("/child%20name.json")),
        ("/child.json?x=1#fragment", Ok("/child.json?x=1")),
        ("/a/%2e%2e/child.json", Err("InvalidURL")),
        ("/child\\name.json", Err("InvalidURL")),
        ("/child.json?x='value'", Err("InvalidURL")),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let targets = Arc::new(std::sync::Mutex::new(vec![]));
        let seen = targets.clone();
        let redirect_location = location.to_string();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let seen = seen.clone();
                let location = redirect_location.clone();
                tokio::spawn(async move {
                    let mut request = [0; 8192];
                    let n = stream.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..n]);
                    let target = request.split(' ').nth(1).unwrap().to_string();
                    seen.lock().unwrap().push(target.clone());
                    let response = if target == "/root.json" {
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    } else {
                        // An empty redirected root succeeds without child GETs.
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()
                    };
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        let catalog = StacCatalog::new(
            format!("http://{address}/root.json"),
            CatalogConfig::default(),
        )
        .unwrap();
        let result = catalog.list_collections().await;
        let targets = targets.lock().unwrap().clone();
        match expected {
            Ok(target) => {
                assert!(result.unwrap().is_empty(), "{location}");
                assert_eq!(targets, ["/root.json", target], "{location}");
            }
            Err(class) => {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    format!("STAC catalog refresh failed ({class})"),
                    "{location}"
                );
                assert_eq!(targets, ["/root.json"], "{location}");
            }
        }
        catalog.close();
        task.abort();
    }
}

/// Distinct valid documents at the intended and rewritten paths expose a wrong
/// snapshot even when both requests would otherwise succeed.
#[tokio::test]
async fn terminal_dot_segments_fetch_the_python_document_in_every_position() {
    for suffix in ["/data/.", "/data/nested/.."] {
        for phase in ["root", "child", "redirect"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let targets = Arc::new(std::sync::Mutex::new(vec![]));
            let seen = targets.clone();
            let destination = format!("http://{address}{suffix}");
            let href = destination.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let (seen, href) = (seen.clone(), href.clone());
                    tokio::spawn(async move {
                        let mut request = [0; 8192];
                        let n = stream.read(&mut request).await.unwrap();
                        let request = String::from_utf8_lossy(&request[..n]);
                        let target = request.split(' ').nth(1).unwrap().to_string();
                        seen.lock().unwrap().push(target.clone());
                        let response = if phase == "redirect" && target == "/root.json" {
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: {href}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            let body = if phase == "child" && target == "/root.json" {
                                serde_json::json!({"links":[{"rel":"child","href":href}]})
                                    .to_string()
                            } else if phase != "child" && target == "/data" {
                                r#"{"links":[{"rel":"child","href":"/expected.json"}]}"#.into()
                            } else {
                                let id = if target == "/data" || target == "/expected.json" {
                                    "expected"
                                } else {
                                    "wrong"
                                };
                                serde_json::json!({"id":id,"assets":{"icechunk":{"href":"https://data.dynamical.org/example/"}}}).to_string()
                            };
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            });
            let url = if phase == "root" {
                destination
            } else {
                format!("http://{address}/root.json")
            };
            let catalog = StacCatalog::new(url, CatalogConfig::default()).unwrap();
            assert_eq!(
                catalog.list_collections().await.unwrap()[0].id,
                "expected",
                "{phase} {suffix}"
            );
            let expected = match phase {
                "root" => vec!["/data", "/expected.json"],
                "child" => vec!["/root.json", "/data"],
                _ => vec!["/root.json", "/data", "/expected.json"],
            };
            assert_eq!(*targets.lock().unwrap(), expected, "{phase} {suffix}");
            catalog.close();
            task.abort();
        }
    }
}

#[tokio::test]
async fn absolute_redirects_preserve_empty_queries_and_decode_location_bytes() {
    for (location, target) in [
        (b"/catalog.json?".as_slice(), "/catalog.json?"),
        (b"/catalog.json?#", "/catalog.json?"),
        (b"/\xe9.json", "/%C3%A9.json"),
        (b"/\xc3\xa9.json", "/%C3%A9.json"),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let targets = Arc::new(std::sync::Mutex::new(vec![]));
        let seen = targets.clone();
        let mut header = if location.starts_with(b"/catalog") {
            format!("http://{address}").into_bytes()
        } else {
            vec![]
        };
        header.extend_from_slice(location);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (seen, header) = (seen.clone(), header.clone());
                tokio::spawn(async move {
                    let mut request = [0; 8192];
                    let n = stream.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..n]);
                    let actual = request.split(' ').nth(1).unwrap().to_string();
                    seen.lock().unwrap().push(actual.clone());
                    let response = if actual == "/root.json" {
                        let mut response = b"HTTP/1.1 302 Found\r\nLocation: ".to_vec();
                        response.extend_from_slice(&header);
                        response.extend_from_slice(
                            b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        response
                    } else {
                        // Wrong targets return a valid but different root.
                        let child = if actual == target {
                            "/expected.json"
                        } else {
                            "/wrong.json"
                        };
                        let body = if actual.ends_with("expected.json")
                            || actual.ends_with("wrong.json")
                        {
                            let id = if actual == "/expected.json" {
                                "expected"
                            } else {
                                "wrong"
                            };
                            serde_json::json!({"id":id,"assets":{"icechunk":{"href":"https://data.dynamical.org/example/"}}}).to_string()
                        } else {
                            serde_json::json!({"links":[{"rel":"child","href":child}]}).to_string()
                        };
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
                    };
                    let _ = stream.write_all(&response).await;
                });
            }
        });
        let catalog = StacCatalog::new(
            format!("http://{address}/root.json"),
            CatalogConfig::default(),
        )
        .unwrap();
        assert_eq!(
            catalog.list_collections().await.unwrap()[0].id,
            "expected",
            "{location:?}"
        );
        assert_eq!(
            *targets.lock().unwrap(),
            ["/root.json", target, "/expected.json"]
        );
        catalog.close();
        task.abort();
    }
}
