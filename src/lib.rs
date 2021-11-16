//! A micro-library for downloading from a URL and streaming it directly to the disk
//!
//! # Getting Started
//!
//! ```rust
//! use std::path::Path;
//!
//! #[tokio::main]
//! async fn main() {
//!     if tokio_dl_stream_to_disk::download("https://bit.ly/3yWXSOW", &Path::new("/tmp"), "5mb_test.bin").await.is_ok() {
//!         println!("File downloaded successfully!");
//!     }
//! }
//! ```

pub mod error;

use std::error::Error;
use std::io::{Error as IOError, ErrorKind};
use std::path::Path;

use bytes::Bytes;
use futures_util::stream::Stream;
use futures_util::StreamExt;

#[cfg(feature="sha256sum")]
use sha2::{Sha256, Digest};
use tokio_util::io::StreamReader;

use crate::error::{Error as TDSTDError, ErrorKind as TDSTDErrorKind};

type S = dyn Stream<Item = Result<Bytes, IOError>> + Unpin;
async fn http_stream(url: &str) -> Result<Box<S>, Box<dyn Error>> {
    Ok(Box::new(reqwest::get(url)
        .await?
        .error_for_status()?
        .bytes_stream()
        .map(|result| result.map_err(|e| IOError::new(ErrorKind::Other, e)))))
}

pub async fn download<S: Into<String>>(url: S, dst_path: &Path, fname: S) -> Result<(), TDSTDError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fname = dst_path.join(fname.into());
    if fname.is_file() {
        return Err(TDSTDError::new(TDSTDErrorKind::FileExists));
    }

    if dst_path.is_dir() {
        let mut http_async_reader = {
            let http_stream = http_stream(&url.into()).await?;
            StreamReader::new(http_stream)
        };

        let mut dest = tokio::fs::File::create(fname).await?;
        let mut buf = [0; 8 * 1024];
        loop {
            let num_bytes = http_async_reader.read(&mut buf).await?;
            if num_bytes > 0 {
                dest.write(&mut buf[0..num_bytes]).await?;
            } else {
                break;
            }
        }
        Ok(())
    } else {
        Err(TDSTDError::new(TDSTDErrorKind::DirectoryMissing))
    }
}

#[cfg(feature="sha256sum")]
pub async fn download_and_return_sha256sum<S: Into<String>>(url: S, dst_path: &Path, fname: S) -> Result<Vec<u8>, TDSTDError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fname = dst_path.join(fname.into());
    if fname.is_file() {
        return Err(TDSTDError::new(TDSTDErrorKind::FileExists));
    }

    if dst_path.is_dir() {
        let mut http_async_reader = {
            let http_stream = http_stream(&url.into()).await?;
            StreamReader::new(http_stream)
        };

        let mut dest = tokio::fs::File::create(fname).await?;
        let mut buf = [0; 8 * 1024];
        let mut hasher = Sha256::new();
        loop {
            let num_bytes = http_async_reader.read(&mut buf).await?;
            if num_bytes > 0 {
                dest.write(&mut buf[0..num_bytes]).await?;
                hasher.update(&buf[0..num_bytes]);
            } else {
                break;
            }
        }
        Ok(hasher.finalize().to_vec())
    } else {
        Err(TDSTDError::new(TDSTDErrorKind::DirectoryMissing))
    }
}
