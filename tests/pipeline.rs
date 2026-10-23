//! End-to-end tests use the real HTTP client against a local Registry v2 fixture.
use flate2::{Compression as GzLevel, read::GzDecoder, write::GzEncoder};
use oci_pull::{model::*, registry};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}
fn desc(bytes: &[u8], media: &str) -> Value {
    json!({"mediaType":media,"digest":digest(bytes),"size":bytes.len()})
}
fn tar_layer(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, name, bytes).unwrap();
    tar.into_inner().unwrap()
}
struct Fixture {
    routes: HashMap<String, (String, Vec<u8>)>,
    raw: Vec<Vec<u8>>,
    blobs: Vec<Vec<u8>>,
    config: Vec<u8>,
    manifest_digest: String,
}
impl Fixture {
    fn new(bad_diff: bool) -> Self {
        let raw = vec![
            tar_layer("hello.txt", b"OCI Pull\n"),
            tar_layer("second.txt", b"second layer\n"),
            tar_layer("last.txt", b"last layer\n"),
        ];
        let mut gzip = GzEncoder::new(Vec::new(), GzLevel::fast());
        gzip.write_all(&raw[0]).unwrap();
        let blobs = vec![
            gzip.finish().unwrap(),
            zstd::stream::encode_all(&raw[1][..], 1).unwrap(),
            raw[2].clone(),
        ];
        let mut diff_ids: Vec<_> = raw.iter().map(|b| digest(b)).collect();
        diff_ids.push(diff_ids[0].clone());
        if bad_diff {
            diff_ids[0] = digest(b"wrong decoded layer");
        }
        let config = serde_json::to_vec(&json!({"architecture":"amd64","os":"linux","config":{"Env":["PATH=/bin"],"Cmd":["/bin/sh"]},"rootfs":{"type":"layers","diff_ids":diff_ids},"history":[{},{},{},{}]})).unwrap();
        let mut layers = vec![
            desc(&blobs[0], "application/vnd.oci.image.layer.v1.tar+gzip"),
            desc(&blobs[1], "application/vnd.oci.image.layer.v1.tar+zstd"),
            desc(&blobs[2], "application/vnd.oci.image.layer.v1.tar"),
        ];
        layers.push(layers[0].clone());
        let manifest = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","annotations":{"example":"retained"},"config":desc(&config,"application/vnd.oci.image.config.v1+json"),"layers":layers})).unwrap();
        let mut descriptor = desc(&manifest, "application/vnd.oci.image.manifest.v1+json");
        descriptor["platform"] = json!({"os":"linux","architecture":"amd64"});
        let index = serde_json::to_vec(&json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[descriptor]})).unwrap();
        let manifest_digest = digest(&manifest);
        let mut routes = HashMap::from([
            ("/v2/".into(), ("application/json".into(), b"{}".to_vec())),
            (
                "/v2/demo/manifests/latest".into(),
                ("application/vnd.oci.image.index.v1+json".into(), index),
            ),
            (
                format!("/v2/demo/manifests/{manifest_digest}"),
                (
                    "application/vnd.oci.image.manifest.v1+json".into(),
                    manifest,
                ),
            ),
        ]);
        for blob in blobs.iter().chain(std::iter::once(&config)) {
            routes.insert(
                format!("/v2/demo/blobs/{}", digest(blob)),
                ("application/octet-stream".into(), blob.clone()),
            );
        }
        Self {
            routes,
            raw,
            blobs,
            config,
            manifest_digest,
        }
    }
}
struct Registry {
    address: String,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}
impl Registry {
    fn new(routes: HashMap<String, (String, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let requests = hits.clone();
        let worker = thread::spawn(move || {
            let routes = Arc::new(routes);
            let mut connections = Vec::new();
            while !done.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // Windows accepted sockets inherit the listener's
                        // nonblocking mode; this fixture reads on worker threads.
                        socket.set_nonblocking(false).unwrap();
                        let routes = routes.clone();
                        let requests = requests.clone();
                        connections.push(thread::spawn(move || {
                        socket
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        loop {
                        let mut request = Vec::new();
                        let mut buffer = [0; 2048];
                        while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                            match socket.read(&mut buffer) {
                                Ok(0) | Err(_) => return,
                                Ok(n) => request.extend_from_slice(&buffer[..n]),
                            }
                        }
                        let request = String::from_utf8_lossy(&request);
                        let path = request.split_whitespace().nth(1).unwrap_or("/");
                        *requests.lock().unwrap().entry(path.into()).or_default() += 1;
                        let (status, media, body) = match routes.get(path) {
                            Some((media, body)) => ("200 OK", media.as_str(), body.as_slice()),
                            None => ("404 Not Found", "application/json", b"{}".as_slice()),
                        };
                        let header = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: {media}\r\nContent-Length: {}\r\nDocker-Content-Digest: {}\r\nDocker-Distribution-Api-Version: registry/2.0\r\nConnection: keep-alive\r\n\r\n",
                            body.len(),
                            digest(body)
                        );
                        let mut response = header.into_bytes();
                        if !request.starts_with("HEAD ") {
                            response.extend_from_slice(body);
                        }
                        if socket.write_all(&response).is_err() {
                            return;
                        }
                        }
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("mock registry: {e}"),
                }
            }
            for connection in connections {
                connection.join().unwrap();
            }
        });
        Self {
            address,
            stop,
            worker: Some(worker),
            hits,
        }
    }
    fn request(&self, root: &Path) -> PullRequest {
        PullRequest {
            image: format!("{}/demo:latest", self.address),
            platform: Platform::parse("linux/amd64").unwrap(),
            format: ArchiveFormat::Docker,
            compression: Compression::Zstd,
            output_dir: root.join("out"),
            file_name: None,
            cache_dir: root.join("cache"),
            manifest_dir: None,
            credentials: Credentials::default(),
            proxy: None,
            allow_http: true,
            concurrency: 3,
            chunk_size_mib: 8,
            compression_level: 3,
        }
    }
}
impl Drop for Registry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}
fn reporter() -> Reporter {
    Arc::new(|_| {})
}

#[test]
fn cli_uses_managed_cache_and_reclaims_it_after_inspect_and_successful_export() {
    let fixture = Fixture::new(false);
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let output = temp.path().join("images");
    for action in ["inspect", "pull"] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_oci-pull-cli"))
            .arg(action)
            .arg(format!("{}/demo:latest", server.address))
            .args(["--http", "--compression", "xz", "--filename", "image"])
            .arg("--cache")
            .arg(&cache)
            .arg("--output")
            .arg(&output)
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env("NO_PROXY", "localhost,127.0.0.1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        for directory in ["previews", "cli", "locks"] {
            let directory = cache.join(directory);
            if directory.exists() {
                assert_eq!(std::fs::read_dir(directory).unwrap().count(), 0);
            }
        }
    }
    let archive = output.join("image.tar.xz");
    let entries = unpack(&archive, Compression::Xz);
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(
        entries[manifest[0]["Config"].as_str().unwrap()],
        fixture.config
    );
    assert!(cache.join(".oci-pull-lock").exists());
    let help = std::process::Command::new(env!("CARGO_BIN_EXE_oci-pull-cli"))
        .args(["pull", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("[default: 16]"));
    assert!(help.contains("[default: 64]"));
}
fn unpack(path: &Path, compression: Compression) -> HashMap<String, Vec<u8>> {
    let file = std::fs::File::open(path).unwrap();
    let reader: Box<dyn Read> = match compression {
        Compression::None => Box::new(file),
        Compression::Gzip => Box::new(GzDecoder::new(file)),
        Compression::Bzip2 => Box::new(bzip2::read::BzDecoder::new(file)),
        Compression::Xz => Box::new(lzma_rust2::XzReader::new(file, false)),
        Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(file).unwrap()),
    };
    let mut tar = tar::Archive::new(reader);
    let mut entries = HashMap::new();
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        assert!(entries.insert(name, bytes).is_none());
    }
    entries
}
fn assert_no_parts(path: &Path) {
    if path.exists() {
        for entry in std::fs::read_dir(path).unwrap() {
            assert!(
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".part")
            );
        }
    }
}

#[tokio::test]
async fn archives_round_trip_and_reuse_validated_cache() {
    let fixture = Fixture::new(false);
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let mut request = server.request(temp.path());
    for format in [ArchiveFormat::Docker, ArchiveFormat::Oci] {
        for compression in [
            Compression::Zstd,
            Compression::Gzip,
            Compression::Bzip2,
            Compression::Xz,
            Compression::None,
        ] {
            request.format = format;
            request.compression = compression;
            let path = registry::pull(request.clone(), reporter(), CancellationToken::new())
                .await
                .unwrap();
            let entries = unpack(&path, compression);
            if format == ArchiveFormat::Docker {
                let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
                assert_eq!(
                    entries[manifest[0]["Config"].as_str().unwrap()],
                    fixture.config
                );
                assert_eq!(manifest[0]["RepoTags"][0], request.image);
                let layers = manifest[0]["Layers"].as_array().unwrap();
                assert_eq!(layers.len(), 4);
                for (i, layer) in layers.iter().enumerate() {
                    assert_eq!(entries[layer.as_str().unwrap()], fixture.raw[i % 3]);
                }
            } else {
                let index: Value = serde_json::from_slice(&entries["index.json"]).unwrap();
                assert_eq!(index["manifests"][0]["platform"]["architecture"], "amd64");
                let id = index["manifests"][0]["digest"].as_str().unwrap();
                let data = &entries[&format!("blobs/sha256/{}", digest_hex(id).unwrap())];
                assert_eq!(digest(data), id);
                let manifest: Value = serde_json::from_slice(data).unwrap();
                assert_eq!(manifest["annotations"]["example"], "retained");
                for blob in fixture.blobs.iter().chain(std::iter::once(&fixture.config)) {
                    assert_eq!(
                        entries[&format!("blobs/sha256/{}", digest_hex(&digest(blob)).unwrap())],
                        *blob
                    );
                }
            }
        }
    }
    for blob in fixture.blobs.iter().chain(std::iter::once(&fixture.config)) {
        assert_eq!(
            server.hits.lock().unwrap()[&format!("/v2/demo/blobs/{}", digest(blob))],
            1
        );
    }
    assert_no_parts(&request.output_dir);
    assert_no_parts(&request.cache_dir);
}

#[tokio::test]
async fn corrupted_cache_is_repaired_and_existing_output_is_preserved() {
    let fixture = Fixture::new(false);
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let request = server.request(temp.path());
    std::fs::create_dir_all(&request.cache_dir).unwrap();
    let blob = &fixture.blobs[0];
    let cached = request.cache_dir.join(digest_hex(&digest(blob)).unwrap());
    std::fs::write(&cached, vec![b'x'; blob.len()]).unwrap();
    let path = registry::pull(request.clone(), reporter(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(cached).unwrap(), *blob);
    let original = std::fs::read(&path).unwrap();
    assert!(
        registry::pull(request, reporter(), CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("已存在")
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[tokio::test]
async fn incorrect_diff_id_never_publishes_an_archive() {
    let fixture = Fixture::new(true);
    let server = Registry::new(fixture.routes);
    let temp = tempfile::tempdir().unwrap();
    let request = server.request(temp.path());
    let error = registry::pull(request.clone(), reporter(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("diff_id"));
    assert_eq!(std::fs::read_dir(&request.output_dir).unwrap().count(), 0);
    assert_no_parts(&request.cache_dir);
}

#[tokio::test]
async fn cancellation_keeps_download_checkpoints_but_removes_partial_archives() {
    let fixture = Fixture::new(false);
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let mut request = server.request(temp.path());
    let plan = registry::inspect(&request, &reporter(), &CancellationToken::new())
        .await
        .unwrap();
    // Download cancellation is independent of the archive encoding.
    let cancel = CancellationToken::new();
    let control = cancel.clone();
    let report: Reporter = Arc::new(move |event| {
        if matches!(event, Event::Layer { .. }) {
            control.cancel();
        }
    });
    let error = registry::pull_resolved(request.clone(), plan.clone(), report, cancel.clone())
        .await
        .unwrap_err();
    assert!(
        cancel.is_cancelled(),
        "download cancellation was not reached: {error:#}"
    );
    assert!(format!("{error:#}").contains("取消"), "{error:#}");
    assert_eq!(std::fs::read_dir(&request.output_dir).unwrap().count(), 0);
    // Exercise each encoder directly with verified fixture data, so a transport
    // failure cannot be mistaken for a successful packaging-cancellation test.
    for blob in fixture.blobs.iter().chain(std::iter::once(&fixture.config)) {
        std::fs::write(
            request.cache_dir.join(digest_hex(&digest(blob)).unwrap()),
            blob,
        )
        .unwrap();
    }
    for compression in [
        Compression::Zstd,
        Compression::Gzip,
        Compression::Bzip2,
        Compression::Xz,
        Compression::None,
    ] {
        request.compression = compression;
        let cancel = CancellationToken::new();
        let control = cancel.clone();
        let report: Reporter = Arc::new(move |event| {
            if matches!(event, Event::Packaging { .. }) {
                control.cancel();
            }
        });
        let error = oci_pull::archive::export(
            &plan,
            &request,
            &request.output_path(&plan.reference),
            &report,
            &cancel,
        )
        .unwrap_err();
        assert!(
            cancel.is_cancelled(),
            "packaging cancellation was not reached ({compression:?}): {error:#}"
        );
        assert!(format!("{error:#}").contains("取消"), "{error:#}");
        assert_eq!(std::fs::read_dir(&request.output_dir).unwrap().count(), 0);
        assert_no_parts(&request.output_dir);
    }
}

#[tokio::test]
async fn platforms_and_digest_only_references_are_respected() {
    let fixture = Fixture::new(false);
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let mut request = server.request(temp.path());
    request.platform = Platform::parse("linux/arm64").unwrap();
    assert!(
        registry::inspect(&request, &reporter(), &CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("可选平台")
    );
    request.platform = Platform::parse("linux/amd64").unwrap();
    request.image = format!("{}/demo@{}", server.address, fixture.manifest_digest);
    let path = registry::pull(request, reporter(), CancellationToken::new())
        .await
        .unwrap();
    let entries = unpack(&path, Compression::Zstd);
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest[0]["RepoTags"], json!([]));
}

#[tokio::test]
async fn corrupted_registry_blob_is_rejected() {
    let mut fixture = Fixture::new(false);
    let path = format!("/v2/demo/blobs/{}", digest(&fixture.blobs[0]));
    fixture.routes.get_mut(&path).unwrap().1[0] ^= 1;
    let server = Registry::new(fixture.routes);
    let temp = tempfile::tempdir().unwrap();
    let request = server.request(temp.path());
    assert!(
        registry::pull(request.clone(), reporter(), CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&request.output_dir).unwrap().count(), 0);
    assert_no_parts(&request.cache_dir);
}

#[tokio::test]
async fn index_variant_is_retained_when_config_omits_it() {
    let mut fixture = Fixture::new(false);
    let index = &mut fixture
        .routes
        .get_mut("/v2/demo/manifests/latest")
        .unwrap()
        .1;
    let mut value: Value = serde_json::from_slice(index).unwrap();
    value["manifests"][0]["platform"]["variant"] = json!("v3");
    *index = serde_json::to_vec(&value).unwrap();
    let server = Registry::new(fixture.routes);
    let temp = tempfile::tempdir().unwrap();
    let mut request = server.request(temp.path());
    request.platform = Platform::parse("linux/amd64/v3").unwrap();
    let plan = registry::inspect(&request, &reporter(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(plan.platform.label(), "linux/amd64/v3");
    request.image = format!("{}/demo@{}", server.address, fixture.manifest_digest);
    assert!(
        registry::inspect(&request, &reporter(), &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn preview_discovers_platforms_and_downloads_the_confirmed_digest() {
    let fixture = Fixture::new(false);
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let mut request = server.request(temp.path());
    let cancel = CancellationToken::new();
    let plan = registry::inspect(&request, &reporter(), &cancel)
        .await
        .unwrap();
    assert_eq!(plan.available_platforms, vec!["linux/amd64"]);
    let tag_requests = server.hits.lock().unwrap()["/v2/demo/manifests/latest"];
    assert_eq!(tag_requests, 1);
    request.compression = Compression::Xz;
    request.file_name = Some(archive_file_name("自定义.tar.gz", request.compression).unwrap());
    request.output_dir = temp.path().join("chosen-after-preview");
    let events: Arc<Mutex<Vec<Event>>> = Arc::default();
    let sink = events.clone();
    let report: Reporter = Arc::new(move |event| sink.lock().unwrap().push(event));
    let path = registry::pull_resolved(request.clone(), plan.clone(), report, cancel)
        .await
        .unwrap();
    {
        let events = events.lock().unwrap();
        let list_index = events
            .iter()
            .position(|e| matches!(e, Event::LayerList(_)))
            .unwrap();
        let first_progress = events
            .iter()
            .position(|e| matches!(e, Event::Layer { .. }))
            .unwrap();
        assert!(list_index < first_progress);
        let Event::LayerList(layers) = &events[list_index] else {
            unreachable!()
        };
        assert_eq!(
            layers.iter().map(|l| &l.digest).collect::<Vec<_>>(),
            plan.manifest
                .layers
                .iter()
                .map(|l| &l.digest)
                .collect::<Vec<_>>()
        );
        for layer in &plan.manifest.layers {
            assert!(events.iter().any(|e| matches!(e, Event::Layer { digest, downloaded, total, .. } if digest == &layer.digest && downloaded == total)));
        }
    }
    assert_eq!(path, request.output_dir.join("自定义.tar.xz"));
    assert_eq!(
        server.hits.lock().unwrap()["/v2/demo/manifests/latest"],
        tag_requests
    );
    let entries = unpack(&path, Compression::Xz);
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(
        entries[manifest[0]["Config"].as_str().unwrap()],
        fixture.config
    );
    let original = std::fs::read(&path).unwrap();
    assert!(
        registry::pull_resolved(request, plan, reporter(), CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("已存在")
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn restored_task_keeps_digest_tag_variant_and_verified_checkpoints() {
    let mut fixture = Fixture::new(false);
    // Resuming must not depend on a mutable source tag still existing.
    fixture.routes.remove("/v2/demo/manifests/latest");
    let server = Registry::new(fixture.routes.clone());
    let temp = tempfile::tempdir().unwrap();
    let cached_blobs = temp
        .path()
        .join("tasks")
        .join(oci_pull::paths::task_directory_name(42))
        .join("blobs");
    std::fs::create_dir_all(&cached_blobs).unwrap();
    for blob in &fixture.blobs {
        std::fs::write(cached_blobs.join(digest_hex(&digest(blob)).unwrap()), blob).unwrap();
    }
    std::fs::write(cached_blobs.join("unrelated"), b"not part of this task").unwrap();
    let directory = temp
        .path()
        .join("tasks")
        .join(oci_pull::paths::task_directory_name(42));
    let mut request = server.request(temp.path());
    request.platform = Platform::parse("linux/amd64/v3").unwrap();
    request.cache_dir = directory.join("blobs");
    request.manifest_dir = Some(directory.join("manifests"));
    let path = registry::pull_pinned(
        request.clone(),
        &fixture.manifest_digest,
        reporter(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(path, request.output_path(&request.image));
    let hits = server.hits.lock().unwrap();
    assert!(!hits.contains_key("/v2/demo/manifests/latest"));
    for blob in &fixture.blobs {
        assert!(!hits.contains_key(&format!("/v2/demo/blobs/{}", digest(blob))));
        assert_eq!(
            std::fs::read(request.cache_dir.join(digest_hex(&digest(blob)).unwrap())).unwrap(),
            *blob
        );
    }
    drop(hits);
    assert!(cached_blobs.join("unrelated").exists());
    let manifests = request.manifest_dir.unwrap();
    let saved = std::fs::read(manifests.join(format!(
        "{}.json",
        digest_hex(&fixture.manifest_digest).unwrap()
    )))
    .unwrap();
    assert_eq!(digest(&saved), fixture.manifest_digest);
    assert_eq!(
        std::fs::read(manifests.join(format!(
            "{}.json",
            digest_hex(&digest(&fixture.config)).unwrap()
        )))
        .unwrap(),
        fixture.config
    );
    let entries = unpack(&path, Compression::Zstd);
    let manifest: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(manifest[0]["RepoTags"], json!([request.image]));
    assert_no_parts(&request.cache_dir);
    assert_no_parts(&manifests);
}
