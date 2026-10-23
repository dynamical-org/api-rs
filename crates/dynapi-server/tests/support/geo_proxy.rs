#![allow(dead_code)]
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Notify, Semaphore},
    task::{JoinHandle, JoinSet},
};

pub const SECRET: &str = "synthetic-geo-secret";
pub const ORIGIN: &str = "synthetic-origin-secret";
pub const BLOCKED: &str = "Local test proxy refuses this caller.\n\nExample terms.";
pub struct State {
    pub requests: Mutex<Vec<BTreeMap<String, String>>>,
    pub status: AtomicU16,
    pub body: Mutex<String>,
    pub hold: AtomicBool,
    pub release: Semaphore,
    pub started: Notify,
    pub chunked: AtomicBool,
}
pub struct Proxy {
    pub url: String,
    pub state: Arc<State>,
    task: JoinHandle<()>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Proxy {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/geo-check", listener.local_addr().unwrap());
        let state = Arc::new(State {
            requests: Mutex::new(vec![]),
            status: AtomicU16::new(0),
            body: Mutex::new(format!(" \n{BLOCKED}\n ")),
            hold: AtomicBool::new(false),
            release: Semaphore::new(0),
            started: Notify::new(),
            chunked: AtomicBool::new(false),
        });
        let state_task = state.clone();
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept() => {
                        let (mut socket, _)=accepted.unwrap(); let state=state_task.clone();
                        tasks.spawn(async move {
                            let mut bytes=Vec::new(); let mut buf=[0;4096];
                            while !bytes.windows(4).any(|w|w==b"\r\n\r\n") {
                                let n=socket.read(&mut buf).await.unwrap(); if n==0 { return; } bytes.extend_from_slice(&buf[..n]);
                            }
                            let raw=String::from_utf8_lossy(&bytes);
                            let mut headers:BTreeMap<String,String>=raw.split("\r\n").skip(1).filter_map(|l|l.split_once(':')).map(|(k,v)|(k.to_lowercase(),v.trim().to_owned())).collect();
                            headers.insert(":request".into(),raw.lines().next().unwrap().into());
                            let status=if headers.get("x-geo-check-secret").map(String::as_str)!=Some(SECRET) {404} else {
                                let mode=state.status.load(Ordering::SeqCst);
                                if mode!=0 {mode} else if headers.get("x-geo-check-country").map(String::as_str)==Some("AA") {204} else {451}
                            };
                            state.requests.lock().unwrap().push(headers); state.started.notify_waiters();
                            if state.hold.load(Ordering::SeqCst) { let _permit=state.release.acquire().await.unwrap(); }
                            let body=if status==204 {String::new()} else {state.body.lock().unwrap().clone()};
                            let response=if state.chunked.load(Ordering::SeqCst) {
                                format!("HTTP/1.1 {status} Test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",body.len())
                            } else {
                                format!("HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nLocation: http://127.0.0.1:1/never-follow\r\nConnection: close\r\n\r\n{body}",body.len())
                            };
                            let _=socket.write_all(response.as_bytes()).await;
                        });
                    }
                    Some(result)=tasks.join_next() => { result.unwrap(); }
                }
            }
        });
        Self { url, state, task }
    }
    pub fn count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }
    pub async fn wait_for(&self, count: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let notified = self.state.started.notified();
                if self.count() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .unwrap();
    }
}
