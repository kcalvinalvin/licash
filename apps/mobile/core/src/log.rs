//! The wallet's log, handed to the app line by line.

use std::{io, sync::Arc};

use tracing_subscriber::EnvFilter;

/// Where the app keeps the wallet's log: on Android, logcat.
#[uniffi::export(with_foreign)]
pub trait Logger: Send + Sync {
    /// One event, formatted, at `level` (`ERROR` to `TRACE`).
    fn log(&self, level: String, line: String);
}

/// Send the log, at `filter` (`info`, `licash_wallet=debug`, ...), to
/// `logger`. Only the first call takes effect.
#[uniffi::export]
pub fn set_logger(logger: Arc<dyn Logger>, filter: String) {
    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .without_time()
        .with_target(true)
        .with_writer(move || Line(logger.clone(), Vec::new()))
        .try_init();
}

/// One event as it is written, handed over whole once done.
struct Line(Arc<dyn Logger>, Vec<u8>);

impl io::Write for Line {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.1.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Line {
    fn drop(&mut self) {
        let line = String::from_utf8_lossy(&self.1);
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let level = line.split_whitespace().next().unwrap_or("INFO");
        self.0.log(level.to_owned(), line.to_owned());
    }
}
