use super::*;
use oci_client::client::{ClientConfig, ClientProtocol};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

#[derive(Clone, Copy, PartialEq)]
enum Behavior {
    Range,
    Ignore,
    WrongRange,
    Reject,
    DropOnce,
    Redirect,
}
const SEGMENT: u64 = 8 * 1024 * 1024;
type RequestLog = Arc<Mutex<Vec<Option<(u64, u64)>>>>;
struct Server {
    image: Reference,
    bytes: Arc<Vec<u8>>,
    desc: Descriptor,
    requests: RequestLog,
    peak: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(size: usize, behavior: Behavior) -> Self {
        let bytes = Arc::new(
            (0..size)
                .map(|i| ((i.wrapping_mul(17) ^ (i / 127)) % 251) as u8)
                .collect::<Vec<_>>(),
        );
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&*bytes)));
        let desc = Descriptor {
            digest,
            size: size as u64,
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            platform: None,
            urls: vec![],
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let image = format!("{address}/demo:latest").parse().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (done, records, data, max) =
            (stop.clone(), requests.clone(), bytes.clone(), peak.clone());
        let digest = desc.digest.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn(move || {
            let mut workers = Vec::new();
            while !done.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let (data, records, active, peak, digest, dropped) = (
                            data.clone(),
                            records.clone(),
                            active.clone(),
                            max.clone(),
                            digest.clone(),
                            dropped.clone(),
                        );
                        workers.push(thread::spawn(move || {
                            loop {
                                let mut request = Vec::new();
                                let mut buffer = [0; 4096];
                                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                                    match socket.read(&mut buffer) {
                                        Ok(0) | Err(_) => return,
                                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                                    }
                                }
                                let request = String::from_utf8_lossy(&request);
                                let path = request.split_whitespace().nth(1).unwrap_or("");
                                if path == "/v2/" {
                                    if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").is_err() { return; }
                                    continue;
                                }
                                if behavior == Behavior::Redirect && path.starts_with("/v2/") {
                                    if write!(socket, "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{address}/store\r\nContent-Length: 0\r\n\r\n").is_err() { return; }
                                    continue;
                                }
                                let range = request.lines().find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    if !key.eq_ignore_ascii_case("range") { return None; }
                                    let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
                                    Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()?))
                                });
                                records.lock().unwrap().push(range);
                                if behavior == Behavior::Reject && range.is_some() {
                                    let body = br#"{"errors":[{"code":"UNSUPPORTED"}]}"#;
                                    if write!(socket, "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: {}\r\n\r\n", body.len()).is_err() || socket.write_all(body).is_err() { return; }
                                    continue;
                                }
                                let partial = range.is_some() && behavior != Behavior::Ignore;
                                let (start, end) = if partial { range.unwrap() } else { (0, data.len() as u64 - 1) };
                                let header_range = if partial { format!("Content-Range: bytes {}-{end}/{}\r\n", start + u64::from(behavior == Behavior::WrongRange), data.len()) } else { String::new() };
                                let length = end - start + 1;
                                let status = if partial { "206 Partial Content" } else { "200 OK" };
                                if write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {length}\r\n{header_range}Docker-Content-Digest: {digest}\r\n\r\n").is_err() { return; }
                                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                                peak.fetch_max(count, Ordering::SeqCst);
                                thread::sleep(Duration::from_millis(30));
                                let abort = behavior == Behavior::DropOnce && !dropped.swap(true, Ordering::SeqCst);
                                let send_end = if abort { start + length / 2 } else { end + 1 };
                                let mut failed = false;
                                for chunk in data[start as usize..send_end as usize].chunks(64 * 1024) {
                                    thread::sleep(Duration::from_millis(5));
                                    if socket.write_all(chunk).is_err() { failed = true; break; }
                                }
                                active.fetch_sub(1, Ordering::SeqCst);
                                if abort || failed { return; }
                            }
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("test registry: {e}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            image,
            bytes,
            desc,
            requests,
            peak,
            stop,
            thread: Some(worker),
        }
    }
    fn client(&self) -> Client {
        Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        })
    }
    async fn download(
        &self,
        directory: &Path,
        report: &Reporter,
        cancel: &CancellationToken,
    ) -> Result<PathBuf> {
        self.scheduled(directory, report, cancel, &Scheduler::new(8, SEGMENT))
            .await
    }
    async fn scheduled(
        &self,
        directory: &Path,
        report: &Reporter,
        cancel: &CancellationToken,
        scheduler: &Arc<Scheduler>,
    ) -> Result<PathBuf> {
        cache_blob_scheduled(
            &self.client(),
            &self.image,
            &self.desc,
            directory,
            report,
            cancel,
            true,
            scheduler,
        )
        .await
    }
    fn partial(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.part", digest_hex(&self.desc.digest).unwrap()))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn quiet() -> Reporter {
    Arc::new(|_| {})
}

#[tokio::test]
async fn live_settings_wake_downloads_and_repartition_only_unstarted_bytes() {
    let server = Server::new((SEGMENT * 3) as usize, Behavior::Range);
    let temp = tempfile::tempdir().unwrap();
    let manager = DownloadManager::new(DownloadSettings {
        concurrency: 1,
        chunk_size_mib: 8,
    })
    .unwrap();
    let scheduler = manager.task();
    let changed = Arc::new(AtomicBool::new(false));
    let control = manager.clone();
    let applied = changed.clone();
    let report: Reporter = Arc::new(move |event| {
        if matches!(event, Event::Layer { downloaded, .. } if downloaded >= 512 * 1024)
            && !applied.swap(true, Ordering::SeqCst)
        {
            control
                .configure(DownloadSettings {
                    concurrency: 4,
                    chunk_size_mib: 1,
                })
                .unwrap();
        }
    });
    let path = server
        .scheduled(temp.path(), &report, &CancellationToken::new(), &scheduler)
        .await
        .unwrap();
    assert!(changed.load(Ordering::SeqCst));
    assert_eq!(std::fs::read(path).unwrap(), *server.bytes);
    assert!((2..=4).contains(&server.peak.load(Ordering::SeqCst)));
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests[0], Some((0, SEGMENT - 1)));
    assert_eq!(requests.len(), 17);
    assert!(
        requests[1..]
            .iter()
            .all(|range| range.is_some_and(|(a, b)| b - a + 1 == 1024 * 1024))
    );
}

#[tokio::test]
async fn sequential_pause_and_restart_uses_the_actual_partial_length() {
    let server = Server::new(2 * 1024 * 1024, Behavior::Range);
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let report: Reporter = Arc::new(move |event| {
        if matches!(event, Event::Layer { downloaded, .. } if downloaded >= 512 * 1024) {
            token.cancel();
        }
    });
    assert!(
        server
            .download(temp.path(), &report, &cancel)
            .await
            .is_err()
    );
    assert!(cancel.is_cancelled());
    let size = std::fs::metadata(server.partial(temp.path()))
        .unwrap()
        .len();
    assert!(size > 0 && size < server.desc.size);
    server.requests.lock().unwrap().clear();
    let path = server
        .download(temp.path(), &quiet(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), *server.bytes);
    assert_eq!(
        server.requests.lock().unwrap()[0],
        Some((size, server.desc.size - 1))
    );
    assert!(!server.partial(temp.path()).exists());
}

#[tokio::test]
async fn mixed_piece_layout_survives_restart_with_different_global_settings() {
    let server = Server::new((SEGMENT * 3) as usize, Behavior::Range);
    let temp = tempfile::tempdir().unwrap();
    let manager = DownloadManager::new(DownloadSettings {
        concurrency: 1,
        chunk_size_mib: 8,
    })
    .unwrap();
    let scheduler = manager.task();
    let control = manager.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let report: Reporter = Arc::new(move |event| {
        if let Event::Layer { downloaded, .. } = event {
            if downloaded >= 512 * 1024 {
                control
                    .configure(DownloadSettings {
                        concurrency: 4,
                        chunk_size_mib: 1,
                    })
                    .unwrap();
            }
            if downloaded >= 3 * 1024 * 1024 {
                token.cancel();
            }
        }
    });
    assert!(
        server
            .scheduled(temp.path(), &report, &cancel, &scheduler)
            .await
            .is_err()
    );
    assert!(cancel.is_cancelled());
    let directory = temp
        .path()
        .join(crate::paths::blob_parts_directory_name(&server.desc.digest).unwrap());
    let (plan, bytes) = pieces::Pieces::load(&directory, server.desc.size)
        .await
        .unwrap();
    assert!(plan.entries.iter().any(|p| p.end - p.start + 1 == SEGMENT));
    assert!(
        plan.entries
            .iter()
            .any(|p| p.end - p.start + 1 == 1024 * 1024)
    );
    let mut expected = Vec::new();
    for (piece, bytes) in plan.entries.iter().zip(bytes) {
        if bytes < piece.end - piece.start + 1 {
            expected.push((piece.start + bytes, piece.end));
        }
    }
    assert!(!expected.is_empty());
    server.requests.lock().unwrap().clear();
    let new_scheduler = Scheduler::new(3, 2 * 1024 * 1024);
    let path = server
        .scheduled(
            temp.path(),
            &quiet(),
            &CancellationToken::new(),
            &new_scheduler,
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), *server.bytes);
    let requests = server.requests.lock().unwrap();
    for range in expected {
        assert!(requests.contains(&Some(range)), "lost checkpoint {range:?}");
    }
    assert!(
        requests
            .iter()
            .any(|range| range.is_some_and(|(a, b)| b - a + 1 == 2 * 1024 * 1024))
    );
    assert!(!directory.exists());
}

#[tokio::test]
async fn range_fallbacks_and_corrupt_partial_never_publish_wrong_bytes() {
    for behavior in [
        Behavior::Ignore,
        Behavior::WrongRange,
        Behavior::Reject,
        Behavior::Range,
        Behavior::Redirect,
    ] {
        let server = Server::new(128 * 1024, behavior);
        let temp = tempfile::tempdir().unwrap();
        let prefix = if behavior == Behavior::Range {
            vec![0; 100]
        } else {
            server.bytes[..100].to_vec()
        };
        std::fs::write(server.partial(temp.path()), prefix).unwrap();
        let path = server
            .download(temp.path(), &quiet(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), *server.bytes);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0], Some((100, server.desc.size - 1)));
        if !matches!(behavior, Behavior::Redirect | Behavior::Ignore) {
            assert!(requests.iter().any(Option::is_none));
        }
    }
}

#[tokio::test]
async fn broken_connections_resume_on_retry_and_complete_parts_need_no_network() {
    let server = Server::new(512 * 1024, Behavior::DropOnce);
    let temp = tempfile::tempdir().unwrap();
    let path = server
        .download(temp.path(), &quiet(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), *server.bytes);
    assert!(
        server
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|r| matches!(r, Some((start, _)) if *start > 0))
    );
    std::fs::rename(path, server.partial(temp.path())).unwrap();
    server.requests.lock().unwrap().clear();
    server
        .download(temp.path(), &quiet(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(server.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn parallel_segments_overlap_and_resume_individual_offsets_after_pause() {
    let server = Server::new((SEGMENT * 3 + 256 * 1024) as usize, Behavior::Range);
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let report: Reporter = Arc::new(move |event| {
        if matches!(event, Event::Layer { downloaded, .. } if downloaded >= 2 * 1024 * 1024) {
            token.cancel();
        }
    });
    assert!(
        server
            .download(temp.path(), &report, &cancel)
            .await
            .is_err()
    );
    assert!(cancel.is_cancelled());
    assert!(server.peak.load(Ordering::SeqCst) >= 2);
    let directory = temp
        .path()
        .join(crate::paths::blob_parts_directory_name(&server.desc.digest).unwrap());
    let mut expected = Vec::new();
    for (start, end) in (0..server.desc.size.div_ceil(SEGMENT))
        .map(|i| (i * SEGMENT, ((i + 1) * SEGMENT).min(server.desc.size) - 1))
    {
        let size = std::fs::metadata(segment_path(&directory, start, end))
            .map(|m| m.len())
            .unwrap_or(0);
        if size < end - start + 1 {
            expected.push((start + size, end));
        }
    }
    assert!(!expected.is_empty());
    server.requests.lock().unwrap().clear();
    let path = server
        .download(temp.path(), &quiet(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), *server.bytes);
    let mut actual: Vec<_> = server
        .requests
        .lock()
        .unwrap()
        .iter()
        .copied()
        .map(Option::unwrap)
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert!(!directory.exists());
}

#[tokio::test]
async fn large_blob_falls_back_when_range_is_ignored() {
    let server = Server::new((2 * SEGMENT) as usize, Behavior::Ignore);
    let temp = tempfile::tempdir().unwrap();
    let path = server
        .download(temp.path(), &quiet(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), *server.bytes);
    assert!(server.requests.lock().unwrap().iter().any(Option::is_none));
    assert!(
        temp.path()
            .join(format!(
                "{}.no-range",
                digest_hex(&server.desc.digest).unwrap()
            ))
            .exists()
    );
}

#[tokio::test]
async fn connection_limit_and_shared_cache_lock_bound_parallel_writes() {
    let server = Server::new(256 * 1024, Behavior::Range);
    let temp = tempfile::tempdir().unwrap();
    let scheduler = Scheduler::new(3, SEGMENT);
    let jobs: Vec<_> = (0..12)
        .map(|i| {
            let directory = temp.path().join(i.to_string());
            let server = &server;
            let scheduler = &scheduler;
            async move {
                server
                    .scheduled(&directory, &quiet(), &CancellationToken::new(), scheduler)
                    .await
                    .unwrap()
            }
        })
        .collect();
    futures_util::future::join_all(jobs).await;
    assert!((2..=3).contains(&server.peak.load(Ordering::SeqCst)));
    server.requests.lock().unwrap().clear();
    let directory = temp.path().join("shared");
    let report = quiet();
    let cancel = CancellationToken::new();
    let (a, b) = tokio::join!(
        server.download(&directory, &report, &cancel),
        server.download(&directory, &report, &cancel)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
