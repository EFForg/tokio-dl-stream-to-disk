use std::future::{poll_fn, Future};
use std::path::Path;
use std::task::Poll;

use tokio_dl_stream_to_disk::error::{Error, ErrorKind};
use tokio_dl_stream_to_disk::AsyncDownload;

#[derive(Clone, Copy)]
enum Method {
    Download,
    #[cfg(feature = "sha256sum")]
    Sha256,
}

impl Method {
    async fn run(self, download: &mut AsyncDownload) -> Result<(), Error> {
        match self {
            Self::Download => download.download(&None).await,
            #[cfg(feature = "sha256sum")]
            Self::Sha256 => download
                .download_and_return_sha256sum(&None)
                .await
                .map(|_| ()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Destination {
    MissingParent,
    ParentIsFile,
    File,
    Directory,
    #[cfg(unix)]
    Symlink,
    #[cfg(unix)]
    DanglingSymlink,
    #[cfg(target_os = "linux")]
    DeviceSymlink,
}

async fn assert_local_rejection(method: Method, case: Destination) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let target = root.join("target.bin");
    let original = b"do not change existing entries or their targets";
    let destination = match case {
        Destination::MissingParent => root.join("missing"),
        Destination::ParentIsFile => {
            let parent = root.join("not-a-directory");
            std::fs::write(&parent, original).unwrap();
            parent
        }
        _ => root.to_path_buf(),
    };
    let path = destination.join("download.bin");
    match case {
        Destination::File => std::fs::write(&path, original).unwrap(),
        Destination::Directory => {
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("sentinel"), original).unwrap();
        }
        #[cfg(unix)]
        Destination::Symlink => {
            std::fs::write(&target, original).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
        }
        #[cfg(unix)]
        Destination::DanglingSymlink => std::os::unix::fs::symlink(&target, &path).unwrap(),
        #[cfg(target_os = "linux")]
        Destination::DeviceSymlink => std::os::unix::fs::symlink("/dev/full", &path).unwrap(),
        _ => {}
    }

    // The first URL cannot connect. The second has a connection observer but no
    // HTTP responder: local validation must complete without even polling HTTP.
    let observer = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    observer.set_nonblocking(true).unwrap();
    let observed_url = format!("http://{}/download", observer.local_addr().unwrap());
    for url in ["http://127.0.0.1:0/unreachable", &observed_url] {
        let mut download = AsyncDownload::new(url, &destination, "download.bin");
        let future = method.run(&mut download);
        tokio::pin!(future);
        let error = poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Ready(result) => Poll::Ready(result.unwrap_err()),
            Poll::Pending => panic!("{case:?}: local rejection must precede HTTP"),
        })
        .await;
        match case {
            Destination::MissingParent | Destination::ParentIsFile => {
                assert!(
                    matches!(error.kind(), ErrorKind::DirectoryMissing),
                    "{error}"
                );
            }
            _ => assert!(matches!(error.kind(), ErrorKind::FileExists), "{error}"),
        }
        assert_eq!(
            observer.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    match case {
        Destination::MissingParent => assert!(!destination.exists()),
        Destination::ParentIsFile => assert_eq!(std::fs::read(destination).unwrap(), original),
        Destination::File => assert_eq!(std::fs::read(path).unwrap(), original),
        Destination::Directory => {
            assert_eq!(std::fs::read(path.join("sentinel")).unwrap(), original)
        }
        #[cfg(unix)]
        Destination::Symlink => {
            assert_eq!(std::fs::read_link(&path).unwrap(), target);
            assert_eq!(std::fs::read(target).unwrap(), original);
        }
        #[cfg(unix)]
        Destination::DanglingSymlink => {
            assert_eq!(std::fs::read_link(&path).unwrap(), target);
            assert!(!target.exists(), "dangling link must not create its target");
        }
        #[cfg(target_os = "linux")]
        Destination::DeviceSymlink => {
            assert_eq!(std::fs::read_link(path).unwrap(), Path::new("/dev/full"))
        }
    }
}

macro_rules! destination_tests {
    ($plain:ident, $sha:ident, $case:ident) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $plain() {
            assert_local_rejection(Method::Download, Destination::$case).await;
        }

        #[cfg(feature = "sha256sum")]
        #[tokio::test(flavor = "current_thread")]
        async fn $sha() {
            assert_local_rejection(Method::Sha256, Destination::$case).await;
        }
    };
}

destination_tests!(
    download_missing_parent_before_http,
    sha256_missing_parent_before_http,
    MissingParent
);
destination_tests!(
    download_parent_file_before_http,
    sha256_parent_file_before_http,
    ParentIsFile
);
destination_tests!(
    download_existing_file_before_http,
    sha256_existing_file_before_http,
    File
);
destination_tests!(
    download_directory_entry_before_http,
    sha256_directory_entry_before_http,
    Directory
);
#[cfg(unix)]
destination_tests!(
    download_symlink_before_http,
    sha256_symlink_before_http,
    Symlink
);
#[cfg(unix)]
destination_tests!(
    download_dangling_symlink_before_http,
    sha256_dangling_symlink_before_http,
    DanglingSymlink
);
#[cfg(target_os = "linux")]
destination_tests!(
    download_device_symlink_before_http,
    sha256_device_symlink_before_http,
    DeviceSymlink
);
