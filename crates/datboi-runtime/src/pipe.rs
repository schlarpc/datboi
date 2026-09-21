//! Bounded in-memory byte pipe connecting operator-tree nodes (the
//! host-side fiber-suspension seam from D51: a streaming guest composes
//! with its consumer through one of these, each on its own thread, with
//! backpressure via the channel bound — never visible to the guest).
//!
//! Error propagation: a producer that fails calls [`PipeHandle::fail`];
//! the reader drains already-queued chunks, then surfaces the error
//! instead of a clean EOF. A dropped reader turns subsequent writes into
//! `BrokenPipe`, which unwinds the producer thread promptly.
//!
//! A failure also carries its KIND across the thread boundary (D126).
//! A guest that traps on these inputs will trap on them again, forever;
//! a full disk will not. Losing that distinction here is what made a
//! deterministic `recreate` panic look environmental to every consumer
//! upstream — so [`PipeHandle::fail_deterministic`] marks the verdict
//! and [`deterministic_cause`] reads it back out of the `io::Error`,
//! through however many layers wrapped it on the way.

use std::io::{self, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};

/// Queue bound in chunks. Writers hand over whole chunks (guests write
/// ≤ MAX_READ-sized pieces; builtins write ≤ 64 KiB), so worst-case
/// buffering is small and fixed per pipe.
const DEPTH: usize = 4;

/// A producer failure that is a property of its INPUTS, not of the
/// machine it ran on (D126): a guest trap, a guest-reported error, a
/// claim the bytes disprove. Re-running it cannot help. Carried as the
/// `io::Error`'s payload so a consumer many layers up can still tell it
/// from a full disk.
#[derive(Debug, Clone)]
pub struct Deterministic(pub String);

impl std::fmt::Display for Deterministic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Deterministic {}

/// The deterministic verdict behind `error`, if there is one.
/// `None` means environmental: retrying may help.
///
/// Both ways an `io::Error` can carry another are followed. `get_ref`
/// holds the payload an `io::Error::other` wrapped — and one
/// `io::Error` wrapped inside another is exactly what a spill produces,
/// so that case recurses. `source` is walked too, for anything that
/// nests by the ordinary `std::error::Error` convention. Depth is
/// bounded: the operator tree has a depth limit and every layer here
/// strips one.
#[must_use]
pub fn deterministic_cause(error: &io::Error) -> Option<&str> {
    if let Some(inner) = error.get_ref() {
        if let Some(d) = inner.downcast_ref::<Deterministic>() {
            return Some(&d.0);
        }
        if let Some(nested) = inner.downcast_ref::<io::Error>() {
            return deterministic_cause(nested);
        }
    }
    let mut source = std::error::Error::source(error);
    while let Some(err) = source {
        if let Some(d) = err.downcast_ref::<Deterministic>() {
            return Some(&d.0);
        }
        source = err.source();
    }
    None
}

#[derive(Default)]
struct State {
    error: Option<String>,
    /// Was that error a property of the inputs (D126)? Only meaningful
    /// beside `error`.
    deterministic: bool,
    /// The producer's verdict is in (success or failure). Readers that
    /// hit channel-disconnect WAIT for this before choosing EOF vs
    /// error: the writer drops inside the producer call, BEFORE the
    /// producer thread can record its failure — deciding at disconnect
    /// time would race a guest trap into a clean-looking short stream
    /// (observed in the wild as "spill produced 0 bytes" poisoning a
    /// perfectly good recipe).
    finished: bool,
}

struct Shared {
    state: Mutex<State>,
    done: Condvar,
}

/// Create a connected (writer, reader, handle) triple. The handle
/// outlives the writer so a failed producer can be distinguished from a
/// finished one after its sinks are gone.
pub fn pipe() -> (PipeWriter, PipeReader, PipeHandle) {
    let (tx, rx) = sync_channel(DEPTH);
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        done: Condvar::new(),
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
    /// Also marks the producer finished. The failure is treated as
    /// ENVIRONMENTAL — a retry may succeed — which is the safe default:
    /// retrying costs time, while wrongly calling a failure permanent
    /// costs the data.
    pub fn fail(&self, message: impl Into<String>) {
        self.fail_with(message, false);
    }

    /// Mark the stream failed with a DETERMINISTIC verdict (D126): the
    /// same inputs through the same code will fail the same way, so a
    /// consumer may settle instead of retrying. The reader surfaces it
    /// as an `io::Error` carrying [`Deterministic`], which
    /// [`deterministic_cause`] recovers.
    pub fn fail_deterministic(&self, message: impl Into<String>) {
        self.fail_with(message, true);
    }

    fn fail_with(&self, message: impl Into<String>, deterministic: bool) {
        let mut state = self.shared.state.lock().expect("pipe mutex");
        if state.error.is_none() {
            state.error = Some(message.into());
            state.deterministic = deterministic;
        }
        state.finished = true;
        self.shared.done.notify_all();
    }

    /// Mark the producer finished (successfully unless `fail` was called).
    pub fn finish(&self) {
        let mut state = self.shared.state.lock().expect("pipe mutex");
        state.finished = true;
        self.shared.done.notify_all();
    }

    /// Guard that calls [`PipeHandle::finish`] on drop — put one at the
    /// top of every producer thread so a panic (or any early exit) still
    /// resolves the reader's wait instead of deadlocking it.
    #[must_use]
    pub fn finish_on_drop(&self) -> FinishGuard {
        FinishGuard {
            handle: self.clone(),
        }
    }
}

pub struct FinishGuard {
    handle: PipeHandle,
}

impl Drop for FinishGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.handle.fail("producer thread panicked");
        } else {
            self.handle.finish();
        }
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
                    // Producer's writer is gone — but its VERDICT may
                    // still be in flight (the writer drops inside the
                    // producer call, before the thread records failure).
                    // Wait for it; only then choose error vs EOF.
                    let mut state = self.shared.state.lock().expect("pipe mutex");
                    while !state.finished {
                        state = self.shared.done.wait(state).expect("pipe mutex");
                    }
                    return match state.error.as_ref() {
                        Some(msg) if state.deterministic => Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            Deterministic(msg.clone()),
                        )),
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
        let (mut w, mut r, h) = pipe();
        let producer = std::thread::spawn(move || {
            let _finished = h.finish_on_drop();
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

    /// D126: the verdict crosses the thread boundary. A deterministic
    /// failure is recognisable to a consumer many layers up — including
    /// through a wrapper that only kept it as a source — while an
    /// ordinary failure stays environmental.
    #[test]
    fn a_deterministic_failure_is_recognisable_upstream() {
        let (w, mut r, h) = pipe();
        h.fail_deterministic("guest trapped in tree_predictor");
        drop(w);
        let err = r.read(&mut [0u8; 4]).expect_err("failure");
        assert_eq!(
            deterministic_cause(&err),
            Some("guest trapped in tree_predictor")
        );
        // Wrapped one layer deeper (the shape a spill produces).
        let wrapped = io::Error::other(err);
        assert_eq!(
            deterministic_cause(&wrapped),
            Some("guest trapped in tree_predictor")
        );

        let (w2, mut r2, h2) = pipe();
        h2.fail("the disk is full");
        drop(w2);
        let env = r2.read(&mut [0u8; 4]).expect_err("failure");
        assert_eq!(deterministic_cause(&env), None, "environmental: retry");
    }

    #[test]
    fn dropped_reader_breaks_writer() {
        let (mut w, r, _h) = pipe();
        drop(r);
        let err = w.write_all(b"x").expect_err("broken pipe");
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
