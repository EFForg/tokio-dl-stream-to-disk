<!--- `README.md` is automatically generated from the rustdoc using [`cargo-readme`](https://crates.io/crates/cargo-readme). -->
# `tokio-dl-stream-to-disk`

[![crates.io](https://img.shields.io/crates/v/tokio-dl-stream-to-disk.svg)](https://crates.io/crates/tokio-dl-stream-to-disk)
[![Documentation](https://docs.rs/tokio-dl-stream-to-disk/badge.svg)](https://docs.rs/tokio-dl-stream-to-disk)
[![MIT licensed](https://img.shields.io/crates/l/tokio-dl-stream-to-disk.svg)](./LICENSE)

A micro-library for downloading from a URL and streaming it directly to the disk

## Getting Started

```rust
use std::path::Path;

#[tokio::main]
async fn main() {
    if tokio_dl_stream_to_disk::download("https://bit.ly/3yWXSOW", &Path::new("/tmp"), "5mb_test.bin").await.is_ok() {
        println!("File downloaded successfully!");
    }
}
```

License: MIT
