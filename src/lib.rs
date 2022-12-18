//! A micro-library for downloading from a URL and streaming it directly to the disk
//!
//! # Getting Started
//!
//! ```rust
//! use std::path::Path;
//! use tokio_dl_stream_to_disk::AsyncDownload;
//!
//! #[tokio::main]
//! async fn main() {
//!     if AsyncDownload::new("https://bit.ly/3yWXSOW", &Path::new("/tmp"), "5mb_test.bin").download(&None).await.is_ok() {
//!         println!("File downloaded successfully!");
//!     }
//! }
//! ```

pub mod error;

use std::convert::TryInto;
use std::error::Error;
use std::io::{Error as IOError, ErrorKind};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::stream::Stream;
use futures_util::StreamExt;

#[cfg(feature="sha256sum")]
use sha2::{Sha256, Digest};
use tokio_util::io::StreamReader;

use crate::error::{Error as TDSTDError, ErrorKind as TDSTDErrorKind};

type S = dyn Stream<Item = Result<Bytes, IOError>> + Unpin;

/// The AsyncDownload struct allows you to stream the contents of a download to the disk.
pub struct AsyncDownload {
    url: String,
    dst_path: PathBuf,
    fname: String,
    length: Option<u64>,
    response_stream: Option<Box<S>>
}

impl AsyncDownload {
    /// Returns an AsyncDownload struct with the url, destination on disk and filename specified.
    ///
    /// # Arguments
    ///
    /// * `url` - A string type containing the URL you want to download the contents of
    /// * `dst_path` - A PathBuf type containing the destination path
    /// * `fname` - A string type containing the filename of the download
    pub fn new(url: &str, dst_path: &Path, fname: &str) -> Self {
        Self {
            url: String::from(url),
            dst_path: PathBuf::from(dst_path),
            fname: String::from(fname),
            length: None,
            response_stream: None
        }
    }

    /// Returns the length of the download in bytes.  This should be called after calling [`get`]
    /// or [`download`].
    pub fn length(&self) -> Option<u64> {
       self.length 
    }

    /// Get the download URL, but do not download it.  If successful, returns an `AsyncDownload`
    /// object with a response stream, which you can then call [`download`] on.  After this, the
    /// length of the download should also be known and you can call [`length`] on it.
    pub async fn get(mut self) -> Result<AsyncDownload, Box<dyn Error>> {
        self.get_non_consumable().await?;
        Ok(self)
    }

    async fn get_non_consumable(&mut self) -> Result<(), Box<dyn Error>> {
        let response = reqwest::get(self.url.clone())
            .await?;
        let content_length = response.headers().get("content-length").map_or(None, 
            |l| {
                match l.to_str() {
                    Err(_) => None,
                    Ok(l_str) => {
                        l_str.parse::<u64>().ok()
                    }
                }
            });
        self.response_stream = Some(Box::new(response
            .error_for_status()?
            .bytes_stream()
            .map(|result| result.map_err(|e| IOError::new(ErrorKind::Other, e)))));
        self.length = content_length;
        Ok(())
    }

    /// Initiate the download and return a result.  Specify an optional callback.
    ///
    /// Arguments:
    /// * `cb` - An optional callback for reporting information about the download asynchronously.
    /// The callback takes the position of the current download, in bytes.
    pub async fn download(&mut self, cb: &Option<Box<dyn Fn(u64) -> ()>>) -> Result<(), TDSTDError> {
        if self.response_stream.is_none() {
            self.get_non_consumable().await.map_err(|_| TDSTDError::new(TDSTDErrorKind::InvalidResponse))?;
        }
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let fname = self.dst_path.join(self.fname.clone());
        if fname.is_file() {
            return Err(TDSTDError::new(TDSTDErrorKind::FileExists));
        }

        if self.dst_path.is_dir() {
            let mut http_async_reader = StreamReader::new(self.response_stream.take().unwrap());

            let mut dest = tokio::fs::File::create(fname).await?;
            let mut buf = [0; 8 * 1024];
            let mut num_bytes_total = 0;
            loop {
                let num_bytes = http_async_reader.read(&mut buf).await?;
                if let Some(ref cb) = cb {
                    num_bytes_total += num_bytes;
                    cb(num_bytes_total.try_into().unwrap());
                }
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
    /// Initiate the download and return a result with the sha256sum of the download contents.
    /// Specify an optional callback.
    ///
    /// Arguments:
    /// * `cb` - An optional callback for reporting information about the download asynchronously.
    /// The callback takes the position of the current download, in bytes.
    pub async fn download_and_return_sha256sum(&mut self, cb: &Option<Box<dyn Fn(u64) -> ()>>) -> Result<Vec<u8>, TDSTDError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let fname = self.dst_path.join(self.fname.clone());
        if fname.is_file() {
            return Err(TDSTDError::new(TDSTDErrorKind::FileExists));
        }

        if self.dst_path.is_dir() {
            let mut http_async_reader = StreamReader::new(self.response_stream.take().unwrap());

            let mut dest = tokio::fs::File::create(fname).await?;
            let mut buf = [0; 8 * 1024];
            let mut num_bytes_total = 0;
            let mut hasher = Sha256::new();
            loop {
                let num_bytes = http_async_reader.read(&mut buf).await?;
                if let Some(ref cb) = cb {
                    num_bytes_total += num_bytes;
                    cb(num_bytes_total.try_into().unwrap());
                }
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
}
