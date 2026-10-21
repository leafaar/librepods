//! What `tracing` prints during a test, for tests about what reaches the log.

use {
    std::{
        io,
        sync::{Arc, Mutex},
    },
    tracing::{Level, subscriber::DefaultGuard},
};

#[derive(Clone, Default)]
pub(crate) struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// Capture the events at `level` and above logged on this thread until
    /// the guard is dropped. A `#[tokio::test]` runs on one thread, so the
    /// capture covers its awaits too.
    pub(crate) fn start(level: Level) -> (Self, DefaultGuard) {
        let capture = Self::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(level)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        (capture, tracing::subscriber::set_default(subscriber))
    }

    /// The captured lines that contain `text`.
    pub(crate) fn lines_with(&self, text: &str) -> Vec<String> {
        let bytes = self.0.lock().unwrap();
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| line.contains(text))
            .map(String::from)
            .collect()
    }
}

impl io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
