use super::*;
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

// Test the actual copy loop used by both public methods, not Tokio internals.
// Two-byte writes and explicit Pending exercise write_all and awaited flush.
struct Writer {
    bytes: Vec<u8>,
    fail_after: Option<usize>,
    zero_write: bool,
    flush_error: bool,
    write_pending: bool,
    flush_pending: bool,
    flush_polls: usize,
}

impl Writer {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            fail_after: None,
            zero_write: false,
            flush_error: false,
            write_pending: true,
            flush_pending: true,
            flush_polls: 0,
        }
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, IOError>> {
        if self.write_pending {
            self.write_pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.write_pending = true;
        if self
            .fail_after
            .is_some_and(|limit| self.bytes.len() >= limit)
        {
            return Poll::Ready(Err(IOError::new(ErrorKind::BrokenPipe, "write failure")));
        }
        if self.zero_write {
            return Poll::Ready(Ok(0));
        }
        let count = buf.len().min(2);
        self.bytes.extend_from_slice(&buf[..count]);
        Poll::Ready(Ok(count))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), IOError>> {
        self.flush_polls += 1;
        if self.flush_pending {
            self.flush_pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if self.flush_error {
            Poll::Ready(Err(IOError::new(
                ErrorKind::BrokenPipe,
                "late flush failure",
            )))
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), IOError>> {
        panic!("the copy loop must flush, not shut down the writer");
    }
}

fn body(bytes: &'static [u8]) -> Box<S> {
    Box::new(futures_util::stream::iter([Ok(Bytes::from_static(bytes))]))
}

type Positions = Rc<RefCell<Vec<u64>>>;
type Callback = Option<Box<dyn Fn(u64)>>;

fn progress() -> (Positions, Callback) {
    let positions = Rc::new(RefCell::new(Vec::new()));
    let observed = Rc::clone(&positions);
    (
        positions,
        Some(Box::new(move |position| {
            observed.borrow_mut().push(position)
        })),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn short_pending_writes_are_completed_and_flushed() {
    let mut dest = Writer::new();
    let (positions, cb) = progress();
    let mut written = Vec::new();
    copy_stream(body(b"abcdef"), &mut dest, &cb, |bytes| {
        written.extend_from_slice(bytes)
    })
    .await
    .unwrap();
    assert_eq!(dest.bytes, b"abcdef");
    assert_eq!(
        written, b"abcdef",
        "observer must see every byte exactly once"
    );
    assert_eq!(dest.flush_polls, 2, "flush must be polled through Pending");
    assert_eq!(
        &*positions.borrow(),
        &[6, 6],
        "progress counts reads and repeats at EOF"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn write_failure_is_propagated_after_a_short_write() {
    let mut dest = Writer::new();
    dest.fail_after = Some(2);
    let (positions, cb) = progress();
    let mut written = Vec::new();
    let error = copy_stream(body(b"abcdef"), &mut dest, &cb, |bytes| {
        written.extend_from_slice(bytes)
    })
    .await
    .unwrap_err();
    let error = error.into_inner_io().unwrap();
    assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "write failure");
    assert_eq!(dest.bytes, b"ab");
    assert!(
        written.is_empty(),
        "partial chunks must not be observed as completed"
    );
    assert_eq!(dest.flush_polls, 0);
    assert_eq!(
        &*positions.borrow(),
        &[6],
        "callback reports bytes read, not bytes written"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn zero_write_is_not_success() {
    let mut dest = Writer::new();
    dest.zero_write = true;
    let error = copy_stream(body(b"abcdef"), &mut dest, &None, |_| {})
        .await
        .unwrap_err();
    assert_eq!(error.into_inner_io().unwrap().kind(), ErrorKind::WriteZero);
    assert!(dest.bytes.is_empty());
    assert_eq!(dest.flush_polls, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn late_flush_failure_is_propagated() {
    let mut dest = Writer::new();
    dest.flush_error = true;
    let (positions, cb) = progress();
    let error = copy_stream(body(b"abcdef"), &mut dest, &cb, |_| {})
        .await
        .unwrap_err();
    let error = error.into_inner_io().unwrap();
    assert_eq!(error.kind(), ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "late flush failure");
    assert_eq!(dest.bytes, b"abcdef");
    assert_eq!(dest.flush_polls, 2);
    assert_eq!(&*positions.borrow(), &[6, 6]);
}

#[tokio::test(flavor = "current_thread")]
async fn empty_body_still_flushes_and_reports_its_failure() {
    let mut dest = Writer::new();
    dest.flush_error = true;
    let (positions, cb) = progress();
    let error = copy_stream(body(b""), &mut dest, &cb, |_| {})
        .await
        .unwrap_err();
    assert_eq!(
        error.into_inner_io().unwrap().to_string(),
        "late flush failure"
    );
    assert!(dest.bytes.is_empty());
    assert_eq!(dest.flush_polls, 2);
    assert_eq!(&*positions.borrow(), &[0]);
}

#[tokio::test(flavor = "current_thread")]
async fn stream_read_error_is_propagated_without_flush() {
    let stream: Box<S> = Box::new(futures_util::stream::iter([
        Ok(Bytes::from_static(b"ab")),
        Err(IOError::new(ErrorKind::UnexpectedEof, "body failure")),
    ]));
    let mut dest = Writer::new();
    let (positions, cb) = progress();
    let error = copy_stream(stream, &mut dest, &cb, |_| {})
        .await
        .unwrap_err();
    assert_eq!(
        error.into_inner_io().unwrap().kind(),
        ErrorKind::UnexpectedEof
    );
    assert_eq!(dest.bytes, b"ab");
    assert_eq!(dest.flush_polls, 0);
    assert_eq!(&*positions.borrow(), &[2]);
}
