//! Bounded in-memory byte pipe connecting operator-tree nodes (the
//! host-side fiber-suspension seam from D51: a streaming guest composes
//! with its consumer through one of these, each on its own thread, with
//! backpressure via the channel bound — never visible to the guest).
//!
//! Error propagation: a producer that fails calls [`PipeHandle::fail`];
//! the reader drains already-queued chunks, then surfaces the error
//! instead of a clean EOF. A dropped reader turns subsequent writes into
//! `BrokenPipe`, which unwinds the producer thread promptly.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

/// Queue bound in chunks. Writers hand over whole chunks (guests write
/// ≤ MAX_READ-sized pieces; builtins write ≤ 64 KiB), so worst-case
/// buffering is small and fixed per pipe.
const DEPTH: usize = 4;

struct Shared {
    error: Mutex<Option<String>>,
}

/// Create a connected (writer, reader, handle) triple. The handle
/// outlives the writer so a failed producer can be distinguished from a
/// finished one after its sinks are gone.
pub fn pipe() -> (PipeWriter, PipeReader, PipeHandle) {
    let (tx, rx) = sync_channel(DEPTH);
    let shared = Arc::new(Shared {
        error: Mutex::new(None),
    });
    (
        PipeWriter { tx },
        PipeReader {
            rx,
            shared: Arc::clone(&shared),
            current: Vec::new(),
            pos: 0,
        },
        PipeHandle { shared },
    )
}

#[derive(Clone)]
pub struct PipeHandle {
    shared: Arc<Shared>,
}

impl PipeHandle {
    /// Mark the stream failed; the reader reports this instead of EOF.
    pub fn fail(&self, message: impl Into<String>) {
        let mut slot = self.shared.error.lock().expect("pipe mutex");
        slot.get_or_insert(message.into());
    }
}

pub struct PipeWriter {
    tx: SyncSender<Vec<u8>>,
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // send() blocks when the queue is full — that IS the backpressure.
        self.tx
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "pipe consumer dropped"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PipeWriter {
    /// Non-blocking probe used only by tests.
    #[cfg(test)]
    fn try_write(&mut self, buf: &[u8]) -> Result<(), std::sync::mpsc::TrySendError<Vec<u8>>> {
        self.tx.try_send(buf.to_vec())
    }
}

pub struct PipeReader {
    rx: Receiver<Vec<u8>>,
    shared: Arc<Shared>,
    current: Vec<u8>,
    pos: usize,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Err(_) => {
                    // Producer gone: failed producers surface their error,
                    // finished ones EOF.
                    let slot = self.shared.error.lock().expect("pipe mutex");
                    return match slot.as_ref() {
                        Some(msg) => Err(io::Error::other(msg.clone())),
                        None => Ok(0),
                    };
                }
            }
        }
        let n = (self.current.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_backpressures() {
        let (mut w, mut r, _h) = pipe();
        let producer = std::thread::spawn(move || {
            for i in 0..100u8 {
                w.write_all(&[i; 1000]).expect("write");
            }
        });
        let mut out = Vec::new();
        r.read_to_end(&mut out).expect("read");
        assert_eq!(out.len(), 100_000);
        assert!(
            out.chunks(1000)
                .enumerate()
                .all(|(i, c)| c.iter().all(|b| *b == i as u8))
        );
        producer.join().expect("producer");
    }

    #[test]
    fn bound_is_finite() {
        let (mut w, _r, _h) = pipe();
        for _ in 0..DEPTH {
            w.try_write(b"x").expect("fits in queue");
        }
        assert!(matches!(
            w.try_write(b"x"),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
    }

    #[test]
    fn failure_beats_eof_and_drains_first() {
        let (mut w, mut r, h) = pipe();
        w.write_all(b"good bytes").expect("write");
        h.fail("guest trapped");
        drop(w);
        let mut buf = [0u8; 10];
        r.read_exact(&mut buf).expect("queued data still readable");
        assert_eq!(&buf, b"good bytes");
        let err = r.read(&mut buf).expect_err("then the failure");
        assert_eq!(err.to_string(), "guest trapped");
    }

    #[test]
    fn dropped_reader_breaks_writer() {
        let (mut w, r, _h) = pipe();
        drop(r);
        let err = w.write_all(b"x").expect_err("broken pipe");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
