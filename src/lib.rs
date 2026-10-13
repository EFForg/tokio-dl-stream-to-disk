//! A micro-library for downloading from a URL and streaming it directly to the disk
//!
//! # Getting Started
//!
//! ```no_run
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

use std::error::Error;
use std::io::{Error as IOError, ErrorKind};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::stream::Stream;
use futures_util::StreamExt;

#[cfg(feature="sha256sum")]
use sha2::{Sha256, Digest};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::StreamReader;

use crate::error::{Error as TDSTDError, ErrorKind as TDSTDErrorKind};

type S = dyn Stream<Item = Result<Bytes, IOError>> + Unpin;

#[cfg(test)]
mod copy_tests;

// Shared by both download methods. The observer allows optional hashing without
// a second copy loop or a public writer API; it sees only fully written chunks.
async fn copy_stream<W: AsyncWrite + Unpin>(
    stream: Box<S>,
    dest: &mut W,
    cb: &Option<Box<dyn Fn(u64)>>,
    mut on_written: impl FnMut(&[u8]),
) -> Result<(), TDSTDError> {
    let mut http_async_reader = StreamReader::new(stream);
    let mut buf = [0; 8 * 1024];
    let mut num_bytes_total = 0u64;
    loop {
        let num_bytes = http_async_reader.read(&mut buf).await?;
        if let Some(ref cb) = cb {
            num_bytes_total += num_bytes as u64;
            cb(num_bytes_total);
        }
        if num_bytes > 0 {
            dest.write_all(&buf[0..num_bytes]).await?;
            on_written(&buf[0..num_bytes]);
        } else {
            break;
        }
    }
    dest.flush().await?;
    Ok(())
}

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

    /// Returns the length of the download in bytes. This should be called after calling
    /// [`get`](Self::get) or [`download`](Self::download).
    pub fn length(&self) -> Option<u64> {
       self.length 
    }

    /// Get the download URL, but do not download it.  If successful, returns an `AsyncDownload`
    /// object with a response stream, which you can then call [`download`](Self::download) on.
    /// After this, the length of the download should also be known and you can call
    /// [`length`](Self::length) on it.
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

    async fn create_destination(&mut self) -> Result<tokio::fs::File, TDSTDError> {
        // Validate local paths before a lazy HTTP request. symlink_metadata also
        // detects dangling links, unlike exists/is_file which follow the link.
        if !self.dst_path.is_dir() {
            return Err(TDSTDError::new(TDSTDErrorKind::DirectoryMissing));
        }
        let fname = self.dst_path.join(&self.fname);
        match std::fs::symlink_metadata(&fname) {
            Ok(_) => return Err(TDSTDError::new(TDSTDErrorKind::FileExists)),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if self.response_stream.is_none() {
            self.get_non_consumable().await.map_err(|_| TDSTDError::new(TDSTDErrorKind::InvalidResponse))?;
        }
        // The early check is only for fast local rejection. Exclusive creation
        // is the authoritative no-overwrite check, including competing symlinks.
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(fname)
            .await
            .map_err(|error| {
                if error.kind() == ErrorKind::AlreadyExists {
                    TDSTDError::new(TDSTDErrorKind::FileExists)
                } else {
                    error.into()
                }
            })
    }

    /// Initiate the download and return a result.  Specify an optional callback.
    ///
    /// A successful return means all downloaded bytes have been written and the file
    /// has been flushed. This does not synchronize the file to disk for crash durability.
    ///
    /// Local destination validation precedes a lazy HTTP request. Any existing
    /// destination entry (including a directory or dangling symlink) returns
    /// `FileExists`; exclusive creation also prevents overwriting a competing entry.
    /// The destination is visible during progress. Errors may leave a partial file;
    /// this method does not publish atomically or delete the path on failure.
    ///
    /// Arguments:
    /// * `cb` - An optional callback receiving the number of bytes read from the response.
    ///   Progress notifications do not imply completed file writes; only a successful return does.
    pub async fn download(&mut self, cb: &Option<Box<dyn Fn(u64) -> ()>>) -> Result<(), TDSTDError> {
        let mut dest = self.create_destination().await?;
        copy_stream(self.response_stream.take().unwrap(), &mut dest, cb, |_| {}).await
    }

    #[cfg(feature="sha256sum")]
    /// Initiate the download and return a result with the sha256sum of the download contents.
    /// Specify an optional callback. Like [`download`](Self::download), this method fetches
    /// the response if [`get`](Self::get) has not been called first.
    ///
    /// A successful return means all downloaded bytes have been written and the file
    /// has been flushed. This does not synchronize the file to disk for crash durability.
    /// The destination validation, exclusive creation, visibility during progress
    /// and possible partial file on failure are the same as [`download`](Self::download).
    /// No digest is returned if reading, writing or flushing fails.
    ///
    /// Arguments:
    /// * `cb` - An optional callback receiving the number of bytes read from the response.
    ///   Progress notifications do not imply completed file writes; only a successful return does.
    pub async fn download_and_return_sha256sum(&mut self, cb: &Option<Box<dyn Fn(u64) -> ()>>) -> Result<Vec<u8>, TDSTDError> {
        let mut dest = self.create_destination().await?;
        let mut hasher = Sha256::new();
        copy_stream(self.response_stream.take().unwrap(), &mut dest, cb, |bytes| hasher.update(bytes)).await?;
        Ok(hasher.finalize().to_vec())
    }
}
