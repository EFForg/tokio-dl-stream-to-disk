use std::cell::RefCell;
use std::future::{poll_fn, Future};
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::task::Poll;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_dl_stream_to_disk::error::{Error, ErrorKind};
use tokio_dl_stream_to_disk::AsyncDownload;

const TIMEOUT: Duration = Duration::from_secs(10);

// One request per fixture, loopback only, with bounded I/O and no detached tasks.
struct Server {
    url: String,
    task: JoinHandle<()>,
}

impl Server {
    async fn response(status: &str, body: &[u8], content_length: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/download", listener.local_addr().unwrap());
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        response.extend_from_slice(body);
        let task = tokio::spawn(async move {
            timeout(TIMEOUT, async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let read = stream.read(&mut buf).await.unwrap();
                    assert_ne!(read, 0, "client closed before sending request headers");
                    request.extend_from_slice(&buf[..read]);
                    assert!(request.len() <= 8192, "request headers too large");
                }
                stream.write_all(&response).await.unwrap();
                stream.shutdown().await.unwrap();
            })
            .await
            .expect("HTTP fixture timed out");
        });
        Self { url, task }
    }

    async fn ok(body: &[u8]) -> Self {
        Self::response("200 OK", body, body.len()).await
    }

    async fn finish(&mut self) {
        timeout(TIMEOUT, &mut self.task)
            .await
            .expect("HTTP fixture did not terminate")
            .expect("HTTP fixture failed");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Hold Tokio's sole blocking worker after the file open and before the first write.
// A buffered write can return immediately, but flush must yield until the worker
// is released. Release on Pending, or inspect the file on Ready *before* release.
// This makes the completion regression deterministic without timing/sleep tricks.
type Release = Rc<RefCell<Option<mpsc::Sender<()>>>>;

fn block_write_worker(release: &Release) -> Option<Box<dyn Fn(u64)>> {
    let release = Rc::clone(release);
    Some(Box::new(move |_| {
        if release.borrow().is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let (entered_sender, entered_receiver) = mpsc::channel();
        tokio::task::spawn_blocking(move || {
            entered_sender.send(()).unwrap();
            // Bounded even if an assertion panics before the gate is released.
            let _ = receiver.recv_timeout(TIMEOUT);
        });
        entered_receiver
            .recv_timeout(TIMEOUT)
            .expect("blocking worker did not enter the gate");
        *release.borrow_mut() = Some(sender);
    }))
}

async fn bytes_at_return<F: Future>(
    future: F,
    release: Release,
    path: &Path,
) -> (F::Output, Vec<u8>) {
    tokio::pin!(future);
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Pending => {
            if let Some(sender) = release.borrow_mut().take() {
                let _ = sender.send(());
            }
            Poll::Pending
        }
        Poll::Ready(result) => {
            let bytes = std::fs::read(path).unwrap();
            if let Some(sender) = release.borrow_mut().take() {
                let _ = sender.send(());
            }
            Poll::Ready((result, bytes))
        }
    })
    .await
}

fn completion_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap()
}

#[test]
fn download_completes_writes_before_returning() {
    completion_runtime().block_on(async {
        timeout(TIMEOUT, async {
            let body = b"small body still buffered by Tokio";
            let mut server = Server::ok(body).await;
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("download.bin");
            let mut download = AsyncDownload::new(&server.url, directory.path(), "download.bin")
                .get()
                .await
                .unwrap();
            server.finish().await;
            let release = Rc::new(RefCell::new(None));
            let callback = block_write_worker(&release);
            let (result, bytes) =
                bytes_at_return(download.download(&callback), release, &path).await;
            result.unwrap();
            assert_eq!(
                bytes, body,
                "successful return must expose every downloaded byte"
            );
        })
        .await
        .expect("download timed out");
    });
}

#[cfg(feature = "sha256sum")]
#[test]
fn sha256_download_completes_writes_before_returning() {
    completion_runtime().block_on(async {
        timeout(TIMEOUT, async {
            let body = b"abc";
            let mut server = Server::ok(body).await;
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("download.bin");
            let mut download = AsyncDownload::new(&server.url, directory.path(), "download.bin")
                .get()
                .await
                .unwrap();
            server.finish().await;
            let release = Rc::new(RefCell::new(None));
            let callback = block_write_worker(&release);
            let (result, bytes) = bytes_at_return(
                download.download_and_return_sha256sum(&callback),
                release,
                &path,
            )
            .await;
            let digest = result.unwrap();
            assert_eq!(
                bytes, body,
                "successful digest return must expose every byte"
            );
            assert_eq!(digest, abc_digest());
        })
        .await
        .expect("SHA256 download timed out");
    });
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_without_get_fetches_response() {
    timeout(TIMEOUT, async {
        let mut server = Server::ok(b"abc").await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("direct.bin");
        let mut download = AsyncDownload::new(&server.url, directory.path(), "direct.bin");
        assert_eq!(download.length(), None);
        let digest = download.download_and_return_sha256sum(&None).await.unwrap();
        assert_eq!(digest, abc_digest());
        assert_eq!(download.length(), Some(3));
        assert_eq!(std::fs::read(path).unwrap(), b"abc");
        server.finish().await;
    })
    .await
    .expect("direct SHA256 download timed out");
}

#[cfg(feature = "sha256sum")]
fn abc_digest() -> [u8; 32] {
    // SHA-256 known-answer vector, independent of the implementation under test.
    [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ]
}

#[derive(Clone, Copy)]
enum Method {
    Download,
    #[cfg(feature = "sha256sum")]
    Sha256,
}

impl Method {
    async fn run(
        self,
        download: &mut AsyncDownload,
        callback: &Option<Box<dyn Fn(u64)>>,
    ) -> Result<Option<Vec<u8>>, Error> {
        match self {
            Self::Download => download.download(callback).await.map(|()| None),
            #[cfg(feature = "sha256sum")]
            Self::Sha256 => download
                .download_and_return_sha256sum(callback)
                .await
                .map(Some),
        }
    }
}

// With a prefetched response there is no await between the early path check
// and the filesystem open. Occupy the only blocking worker, then manually poll
// to enqueue that open. A competing entry is installed before releasing it.
// No sleeps or scheduler races are involved, and the production open is used.
#[derive(Clone, Copy)]
enum CompetingEntry {
    File,
    Directory,
    #[cfg(unix)]
    Symlink,
    #[cfg(unix)]
    DanglingSymlink,
}

async fn assert_competing_creation(method: Method, entry: CompetingEntry) {
    let mut server = Server::ok(b"abc").await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("download.bin");
    let mut download = prepare(&server.url, directory.path(), true).await;
    server.finish().await;
    let (release, wait) = mpsc::channel();
    let (entered, worker_entered) = mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        entered.send(()).unwrap();
        let _ = wait.recv_timeout(TIMEOUT);
    });
    worker_entered.recv_timeout(TIMEOUT).unwrap();
    let mut release = Some(release);
    let callback = None;
    let future = method.run(&mut download, &callback);
    tokio::pin!(future);
    let original = b"created by another downloader";
    #[cfg(unix)]
    let target = directory.path().join("target.bin");
    let result = timeout(
        TIMEOUT,
        poll_fn(|cx| {
            let result = future.as_mut().poll(cx);
            if let Some(release) = release.take() {
                assert!(result.is_pending(), "open must wait for the gated worker");
                assert!(
                    !path.exists(),
                    "destination must be absent at the early check"
                );
                match entry {
                    CompetingEntry::File => std::fs::write(&path, original).unwrap(),
                    CompetingEntry::Directory => {
                        std::fs::create_dir(&path).unwrap();
                        std::fs::write(path.join("sentinel"), original).unwrap();
                    }
                    #[cfg(unix)]
                    CompetingEntry::Symlink => {
                        std::fs::write(&target, original).unwrap();
                        std::os::unix::fs::symlink(&target, &path).unwrap();
                    }
                    #[cfg(unix)]
                    CompetingEntry::DanglingSymlink => {
                        std::os::unix::fs::symlink(&target, &path).unwrap();
                    }
                }
                release.send(()).unwrap();
            }
            result
        }),
    )
    .await
    .expect("competing creation test timed out");
    worker.await.unwrap();
    match entry {
        CompetingEntry::File => assert_eq!(std::fs::read(&path).unwrap(), original),
        CompetingEntry::Directory => {
            assert_eq!(std::fs::read(path.join("sentinel")).unwrap(), original);
        }
        #[cfg(unix)]
        CompetingEntry::Symlink => {
            assert_eq!(std::fs::read_link(&path).unwrap(), target);
            assert_eq!(std::fs::read(&target).unwrap(), original);
        }
        #[cfg(unix)]
        CompetingEntry::DanglingSymlink => {
            assert_eq!(std::fs::read_link(&path).unwrap(), target);
            assert!(
                !target.exists(),
                "competing dangling target must not be created"
            );
        }
    }
    assert!(matches!(result.unwrap_err().kind(), ErrorKind::FileExists));
}

#[test]
fn download_rejects_competing_creation() {
    completion_runtime().block_on(assert_competing_creation(
        Method::Download,
        CompetingEntry::File,
    ));
}

#[cfg(feature = "sha256sum")]
#[test]
fn sha256_download_rejects_competing_creation() {
    completion_runtime().block_on(assert_competing_creation(
        Method::Sha256,
        CompetingEntry::File,
    ));
}

macro_rules! competing_tests {
    ($plain:ident, $sha:ident, $entry:ident) => {
        #[test]
        fn $plain() {
            completion_runtime().block_on(assert_competing_creation(
                Method::Download,
                CompetingEntry::$entry,
            ));
        }
        #[cfg(feature = "sha256sum")]
        #[test]
        fn $sha() {
            completion_runtime().block_on(assert_competing_creation(
                Method::Sha256,
                CompetingEntry::$entry,
            ));
        }
    };
}

competing_tests!(
    download_rejects_competing_directory,
    sha256_rejects_competing_directory,
    Directory
);
#[cfg(unix)]
competing_tests!(
    download_rejects_competing_symlink,
    sha256_rejects_competing_symlink,
    Symlink
);
#[cfg(unix)]
competing_tests!(
    download_rejects_competing_dangling_symlink,
    sha256_rejects_competing_dangling_symlink,
    DanglingSymlink
);

#[derive(Clone, Copy)]
enum Payload {
    Small,
    Large,
    Empty,
}

impl Payload {
    fn bytes(self) -> Vec<u8> {
        match self {
            Self::Small => b"abc".to_vec(),
            // Many 8 KiB library buffers plus a non-aligned final write.
            Self::Large => (0..(32 * 8192 + 31)).map(|i| (i % 256) as u8).collect(),
            Self::Empty => Vec::new(),
        }
    }

    #[cfg(feature = "sha256sum")]
    fn digest(self) -> Vec<u8> {
        let hex = match self {
            Self::Small => return abc_digest().to_vec(),
            Self::Large => "3187a83bdcbb5e9f5a814ea8c89bf899c185e9150db3450af1c7d500d0cf742e",
            Self::Empty => "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        };
        // Constants verified independently with Python hashlib, not sha2.
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }
}

async fn prepare(url: &str, directory: &Path, prefetched: bool) -> AsyncDownload {
    let download = AsyncDownload::new(url, directory, "download.bin");
    assert_eq!(download.length(), None);
    if prefetched {
        download.get().await.unwrap()
    } else {
        download
    }
}

async fn assert_success(method: Method, payload: Payload) {
    for prefetched in [false, true] {
        for with_callback in [false, true] {
            timeout(TIMEOUT, async {
                let body = payload.bytes();
                let mut server = Server::ok(&body).await;
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("download.bin");
                let mut download = prepare(&server.url, directory.path(), prefetched).await;
                if prefetched {
                    assert_eq!(download.length(), Some(body.len() as u64));
                    assert!(!path.exists(), "get must not create the destination");
                }
                let positions = Rc::new(RefCell::new(Vec::new()));
                let callback: Option<Box<dyn Fn(u64)>> = if with_callback {
                    let positions = Rc::clone(&positions);
                    let path = path.clone();
                    Some(Box::new(move |position| {
                        assert!(
                            path.is_file(),
                            "destination must be visible during progress"
                        );
                        positions.borrow_mut().push(position)
                    }))
                } else {
                    None
                };
                let digest = method.run(&mut download, &callback).await.unwrap();
                // Synchronous read immediately on return: no yields or server wait first.
                assert_eq!(std::fs::read(&path).unwrap(), body);
                assert_eq!(download.length(), Some(body.len() as u64));
                match method {
                    Method::Download => assert_eq!(digest, None),
                    #[cfg(feature = "sha256sum")]
                    Method::Sha256 => assert_eq!(digest.unwrap(), payload.digest()),
                }
                {
                    let positions = positions.borrow();
                    if with_callback {
                        assert_eq!(positions.last(), Some(&(body.len() as u64)));
                        if body.is_empty() {
                            assert_eq!(&*positions, &[0]);
                        } else {
                            assert!(positions.len() >= 2);
                            assert_eq!(
                                positions[positions.len() - 2],
                                body.len() as u64,
                                "EOF must repeat the final bytes-read notification"
                            );
                        }
                        assert!(positions.windows(2).all(|pair| pair[0] <= pair[1]));
                        assert!(positions
                            .iter()
                            .all(|&position| position <= body.len() as u64));
                        if body.len() > 8192 {
                            assert!(positions.windows(2).any(|pair| pair[0] < pair[1]));
                        }
                    } else {
                        assert!(positions.is_empty());
                    }
                }
                server.finish().await;
            })
            .await
            .expect("successful download test timed out");
        }
    }
}

async fn assert_destination_error(method: Method, existing_file: bool) {
    for prefetched in [false, true] {
        timeout(TIMEOUT, async {
            let mut server = Server::ok(b"abc").await;
            let directory = tempfile::tempdir().unwrap();
            let destination = if existing_file {
                directory.path().to_path_buf()
            } else {
                directory.path().join("missing")
            };
            let path = destination.join("download.bin");
            let original = b"do not overwrite me";
            if existing_file {
                std::fs::write(&path, original).unwrap();
            }
            let mut download = prepare(&server.url, &destination, prefetched).await;
            let positions = Rc::new(RefCell::new(Vec::new()));
            let observed = Rc::clone(&positions);
            let callback: Option<Box<dyn Fn(u64)>> = Some(Box::new(move |position| {
                observed.borrow_mut().push(position)
            }));
            let error = method.run(&mut download, &callback).await.unwrap_err();
            if existing_file {
                assert!(matches!(error.kind(), ErrorKind::FileExists));
                assert_eq!(std::fs::read(path).unwrap(), original);
            } else {
                assert!(matches!(error.kind(), ErrorKind::DirectoryMissing));
                assert!(
                    !destination.exists(),
                    "missing directory must not be created"
                );
            }
            assert!(positions.borrow().is_empty());
            if prefetched {
                server.finish().await;
            }
        })
        .await
        .expect("destination error test timed out");
    }
}

async fn assert_http_failure(method: Method) {
    for status in ["404 Not Found", "500 Internal Server Error"] {
        timeout(TIMEOUT, async {
            let mut server = Server::response(status, b"error", 5).await;
            let directory = tempfile::tempdir().unwrap();
            let mut download = prepare(&server.url, directory.path(), false).await;
            let error = method.run(&mut download, &None).await.unwrap_err();
            assert!(matches!(error.kind(), ErrorKind::InvalidResponse));
            assert!(!directory.path().join("download.bin").exists());
            server.finish().await;
        })
        .await
        .expect("HTTP failure test timed out");
    }
}

async fn assert_truncated_body(method: Method) {
    for prefetched in [false, true] {
        timeout(TIMEOUT, async {
            let body = b"truncated response";
            let advertised = body.len() + 100;
            let mut server = Server::response("200 OK", body, advertised).await;
            let directory = tempfile::tempdir().unwrap();
            let mut download = prepare(&server.url, directory.path(), prefetched).await;
            let error = method.run(&mut download, &None).await.unwrap_err();
            assert!(
                matches!(error.kind(), ErrorKind::IO(_)),
                "unexpected error: {error}"
            );
            assert_eq!(download.length(), Some(advertised as u64));
            // Failed downloads may leave a partial file; no completed digest is returned.
            assert!(directory.path().join("download.bin").is_file());
            server.finish().await;
        })
        .await
        .expect("truncated body test timed out");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn download_small_body() {
    assert_success(Method::Download, Payload::Small).await;
}

#[tokio::test(flavor = "current_thread")]
async fn download_large_body() {
    assert_success(Method::Download, Payload::Large).await;
}

#[tokio::test(flavor = "current_thread")]
async fn download_empty_body() {
    assert_success(Method::Download, Payload::Empty).await;
}

#[tokio::test(flavor = "current_thread")]
async fn download_does_not_overwrite_existing_file() {
    assert_destination_error(Method::Download, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn download_rejects_missing_directory() {
    assert_destination_error(Method::Download, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn download_rejects_http_failure() {
    assert_http_failure(Method::Download).await;
}

#[tokio::test(flavor = "current_thread")]
async fn download_rejects_truncated_body() {
    assert_truncated_body(Method::Download).await;
}

#[tokio::test(flavor = "current_thread")]
async fn get_rejects_http_failure_without_creating_file() {
    for (status, code) in [("404 Not Found", 404), ("500 Internal Server Error", 500)] {
        timeout(TIMEOUT, async {
            let mut server = Server::response(status, b"error", 5).await;
            let directory = tempfile::tempdir().unwrap();
            let error = AsyncDownload::new(&server.url, directory.path(), "download.bin")
                .get()
                .await
                .err()
                .expect("get must reject an HTTP error status");
            let error = error.downcast_ref::<reqwest::Error>().unwrap();
            assert_eq!(error.status().unwrap().as_u16(), code);
            assert!(!directory.path().join("download.bin").exists());
            server.finish().await;
        })
        .await
        .expect("get HTTP failure test timed out");
    }
}

#[cfg(target_os = "linux")]
async fn assert_device_symlink_rejected(method: Method) {
    use std::os::unix::fs::symlink;

    // Existing symlinks must never be followed, even to writable devices.
    // Write/flush error coverage lives in the shared production copy helper tests.
    timeout(TIMEOUT, async {
        let mut server = Server::ok(b"abc").await;
        let directory = tempfile::tempdir().unwrap();
        symlink("/dev/full", directory.path().join("download.bin")).unwrap();
        let mut download = prepare(&server.url, directory.path(), true).await;
        let error = method.run(&mut download, &None).await.unwrap_err();
        assert!(matches!(error.kind(), ErrorKind::FileExists));
        assert_eq!(
            std::fs::read_link(directory.path().join("download.bin")).unwrap(),
            Path::new("/dev/full")
        );
        server.finish().await;
    })
    .await
    .expect("device symlink rejection test timed out");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn download_rejects_prefetched_device_symlink() {
    assert_device_symlink_rejected(Method::Download).await;
}

#[cfg(all(target_os = "linux", feature = "sha256sum"))]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_rejects_prefetched_device_symlink() {
    assert_device_symlink_rejected(Method::Sha256).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_small_body() {
    assert_success(Method::Sha256, Payload::Small).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_large_body() {
    assert_success(Method::Sha256, Payload::Large).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_empty_body() {
    assert_success(Method::Sha256, Payload::Empty).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_does_not_overwrite_existing_file() {
    assert_destination_error(Method::Sha256, true).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_rejects_missing_directory() {
    assert_destination_error(Method::Sha256, false).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_rejects_http_failure() {
    assert_http_failure(Method::Sha256).await;
}

#[cfg(feature = "sha256sum")]
#[tokio::test(flavor = "current_thread")]
async fn sha256_download_rejects_truncated_body() {
    assert_truncated_body(Method::Sha256).await;
}
