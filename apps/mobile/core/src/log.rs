//! The wallet's log, handed to the app line by line.

use std::{io, sync::Arc};

use tracing_subscriber::EnvFilter;

/// Where the app keeps the wallet's log: on Android, logcat.
#[uniffi::export(with_foreign)]
pub trait Logger: Send + Sync {
    /// One event at `level` (`ERROR` to `TRACE`), formatted after it.
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

/// What a call the app makes took, logged as it returns: at INFO from a
/// second on, where the owner may be waiting, at DEBUG otherwise.
pub(crate) struct Timed(&'static str, std::time::Instant);

/// A [`Timed`] for the call `what`, to keep until it returns.
pub(crate) fn timed(what: &'static str) -> Timed {
    Timed(what, std::time::Instant::now())
}

impl Drop for Timed {
    fn drop(&mut self) {
        let took = self.1.elapsed();
        if took >= std::time::Duration::from_secs(1) {
            tracing::info!(call = self.0, ?took, "took");
        } else {
            tracing::debug!(call = self.0, ?took, "took");
        }
    }
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
        let (level, line) = line.split_once(' ').unwrap_or(("INFO", line));
        self.0.log(level.to_owned(), line.trim_start().to_owned());
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, sync::Mutex};

    use super::*;

    #[derive(Default)]
    struct Kept(Mutex<Vec<(String, String)>>);

    impl Logger for Kept {
        fn log(&self, level: String, line: String) {
            self.0.lock().unwrap().push((level, line));
        }
    }

    #[test]
    fn each_event_is_handed_over_once_its_level_apart() {
        let kept = Arc::new(Kept::default());
        for written in [" WARN licash: a hub is down\n", "\n"] {
            let mut line = Line(kept.clone(), Vec::new());
            line.write_all(written.as_bytes()).unwrap();
        }
        let kept = kept.0.lock().unwrap();
        assert_eq!(*kept, [("WARN".into(), "licash: a hub is down".into())]);
    }
}
