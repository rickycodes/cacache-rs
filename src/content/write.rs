use std::fs::DirBuilder;
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use async_std::fs as afs;
use async_std::future::Future;
use async_std::task::{blocking, Context, JoinHandle, Poll};
use futures::io::AsyncWrite;
use futures::prelude::*;
use ssri::{Algorithm, Integrity, IntegrityOpts};
use tempfile::NamedTempFile;

use crate::content::path;
use crate::errors::Error;

pub struct Writer {
    cache: PathBuf,
    builder: IntegrityOpts,
    tmpfile: NamedTempFile,
}

impl Writer {
    pub fn new(cache: &Path, algo: Algorithm) -> Result<Writer, Error> {
        let cache_path = cache.to_path_buf();
        let mut tmp_path = cache_path.clone();
        tmp_path.push("tmp");
        DirBuilder::new().recursive(true).create(&tmp_path)?;
        Ok(Writer {
            cache: cache_path,
            builder: IntegrityOpts::new().algorithm(algo),
            tmpfile: NamedTempFile::new_in(tmp_path)?,
        })
    }

    pub fn close(self) -> Result<Integrity, Error> {
        let sri = self.builder.result();
        let cpath = path::content_path(&self.cache, &sri);
        DirBuilder::new()
            .recursive(true)
            // Safe unwrap. cpath always has multiple segments
            .create(cpath.parent().unwrap())?;
        self.tmpfile.persist(cpath)?;
        Ok(sri)
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.builder.input(&buf);
        self.tmpfile.write(&buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.tmpfile.flush()
    }
}

pub struct AsyncWriter(Mutex<State>);

enum State {
    Idle(Option<Inner>),
    Busy(JoinHandle<State>),
}

struct Inner {
    cache: PathBuf,
    builder: IntegrityOpts,
    tmpfile: NamedTempFile,
    buf: Vec<u8>,
    last_op: Option<Operation>,
}

enum Operation {
    Write(std::io::Result<usize>),
    Flush(std::io::Result<()>),
}

impl AsyncWriter {
    #[allow(clippy::new_ret_no_self)]
    #[allow(clippy::needless_lifetimes)]
    pub async fn new(cache: &Path, algo: Algorithm) -> Result<AsyncWriter, Error> {
        let cache_path = cache.to_path_buf();
        let mut tmp_path = cache_path.clone();
        tmp_path.push("tmp");
        afs::DirBuilder::new()
            .recursive(true)
            .create(&tmp_path)
            .await?;
        Ok(AsyncWriter(Mutex::new(State::Idle(Some(Inner {
            cache: cache_path,
            builder: IntegrityOpts::new().algorithm(algo),
            tmpfile: blocking(async move { NamedTempFile::new_in(tmp_path) }).await?,
            buf: vec![],
            last_op: None,
        })))))
    }

    pub async fn close(self) -> Result<Integrity, Error> {
        // NOTE: How do I even get access to `inner` safely???
        // let inner = ???;
        // Blocking, but should be a very fast op.
        futures::future::poll_fn(|cx| {
            let state = &mut *self.0.lock().unwrap();

            loop {
                match state {
                    State::Idle(opt) => match opt.take() {
                        None => return Poll::Ready(None),
                        Some(inner) => {
                            let (s, r) = futures::channel::oneshot::channel();
                            let tmpfile = inner.tmpfile;
                            let sri = inner.builder.result();
                            let cpath = path::content_path(&inner.cache, &sri);

                            // Start the operation asynchronously.
                            *state = State::Busy(blocking(async move {
                                let res = afs::DirBuilder::new()
                                    .recursive(true)
                                    // Safe unwrap. cpath always has multiple segments
                                    .create(cpath.parent().unwrap())
                                    .await
                                    .map_err(Error::Io);
                                if res.is_err() {
                                    let _ = s.send(res.map(|_| sri));
                                } else {
                                    let res = tmpfile.persist(cpath);
                                    let res = res.map_err(Error::PersistError);
                                    let _ = s.send(res.map(|_| sri));
                                }
                                State::Idle(None)
                            }));

                            return Poll::Ready(Some(r));
                        }
                    },
                    // Poll the asynchronous operation the file is currently blocked on.
                    State::Busy(task) => *state = futures::ready!(Pin::new(task).poll(cx)),
                }
            }
        })
        .map(|opt| opt.ok_or_else(|| io_error("file closed")))
        .await?
        .map_err(|_| Error::from(io_error("blocking task failed")))
        .await?
    }
}

impl AsyncWrite for AsyncWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let state = &mut *self.0.lock().unwrap();

        loop {
            match state {
                State::Idle(opt) => {
                    // Grab a reference to the inner representation of the file or return an error
                    // if the file is closed.
                    let inner = opt.as_mut().ok_or_else(|| io_error("file closed"))?;

                    // Check if the operation has completed.
                    if let Some(Operation::Write(res)) = inner.last_op.take() {
                        let n = res?;

                        // If more data was written than is available in the buffer, let's retry
                        // the write operation.
                        if n <= buf.len() {
                            return Poll::Ready(Ok(n));
                        }
                    } else {
                        let mut inner = opt.take().unwrap();

                        // Set the length of the inner buffer to the length of the provided buffer.
                        if inner.buf.len() < buf.len() {
                            inner.buf.reserve(buf.len() - inner.buf.len());
                        }
                        unsafe {
                            inner.buf.set_len(buf.len());
                        }

                        // Copy the data to write into the inner buffer.
                        inner.buf[..buf.len()].copy_from_slice(buf);

                        // Start the operation asynchronously.
                        *state = State::Busy(blocking(async move {
                            inner.builder.input(&inner.buf);
                            let res = inner.tmpfile.write(&inner.buf);
                            inner.last_op = Some(Operation::Write(res));
                            State::Idle(Some(inner))
                        }));
                    }
                }
                // Poll the asynchronous operation the file is currently blocked on.
                State::Busy(task) => *state = futures::ready!(Pin::new(task).poll(cx)),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let state = &mut *self.0.lock().unwrap();

        loop {
            match state {
                State::Idle(opt) => {
                    // Grab a reference to the inner representation of the file or return if the
                    // file is closed.
                    let inner = match opt.as_mut() {
                        None => return Poll::Ready(Ok(())),
                        Some(s) => s,
                    };

                    // Check if the operation has completed.
                    if let Some(Operation::Flush(res)) = inner.last_op.take() {
                        return Poll::Ready(res);
                    } else {
                        let mut inner = opt.take().unwrap();

                        // Start the operation asynchronously.
                        *state = State::Busy(blocking(async move {
                            let res = inner.tmpfile.flush();
                            inner.last_op = Some(Operation::Flush(res));
                            State::Idle(Some(inner))
                        }));
                    }
                }
                // Poll the asynchronous operation the file is currently blocked on.
                State::Busy(task) => *state = futures::ready!(Pin::new(task).poll(cx)),
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let state = &mut *self.0.lock().unwrap();

        loop {
            match state {
                State::Idle(opt) => {
                    // Grab a reference to the inner representation of the file or return if the
                    // file is closed.
                    let inner = match opt.take() {
                        None => return Poll::Ready(Ok(())),
                        Some(s) => s,
                    };

                    // Start the operation asynchronously.
                    *state = State::Busy(blocking(async move {
                        drop(inner);
                        State::Idle(None)
                    }));
                }
                // Poll the asynchronous operation the file is currently blocked on.
                State::Busy(task) => *state = futures::ready!(Pin::new(task).poll(cx)),
            }
        }
    }
}

fn io_error(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::task;
    use tempfile;
    #[test]
    fn basic_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        let mut writer = Writer::new(&dir, Algorithm::Sha256).unwrap();
        writer.write_all(b"hello world").unwrap();
        let sri = writer.close().unwrap();
        assert_eq!(sri.to_string(), Integrity::from(b"hello world").to_string());
        assert_eq!(
            std::fs::read(path::content_path(&dir, &sri)).unwrap(),
            b"hello world"
        );
    }

    #[test]
    fn basic_async_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_owned();
        task::block_on(async {
            let mut writer = AsyncWriter::new(&dir, Algorithm::Sha256).await.unwrap();
            writer.write_all(b"hello world").await.unwrap();
            let sri = writer.close().await.unwrap();
            assert_eq!(sri.to_string(), Integrity::from(b"hello world").to_string());
            assert_eq!(
                std::fs::read(path::content_path(&dir, &sri)).unwrap(),
                b"hello world"
            );
        });
    }
}
