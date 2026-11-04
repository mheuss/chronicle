//! Chooses the daemon's log backend: unified logging when
//! `CHRONICLE_LOG_BACKEND=oslog`, otherwise env_logger on stderr.

pub const LOG_BACKEND_ENV: &str = "CHRONICLE_LOG_BACKEND";
const OSLOG_SUBSYSTEM: &str = "dev.heuss.chronicle";

#[derive(Debug, PartialEq, Eq)]
enum Backend {
    Stderr,
    OsLog,
}

fn backend(value: Option<&str>) -> Backend {
    match value {
        Some("oslog") => Backend::OsLog,
        _ => Backend::Stderr,
    }
}

/// Installs the global logger. `RUST_LOG` overrides `default_filter` on both
/// backends.
pub fn init(default_filter: &str) {
    match backend(std::env::var(LOG_BACKEND_ENV).ok().as_deref()) {
        Backend::Stderr => env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or(default_filter),
        )
        .init(),
        Backend::OsLog => {
            chronicle_audio::set_device_identity_in_logs(false);
            let filter = build_filter(std::env::var("RUST_LOG").ok().as_deref(), default_filter);
            // set_boxed_logger leaves the global maximum level at Off.
            log::set_max_level(filter.filter());
            let _ = log::set_boxed_logger(Box::new(Filtered {
                filter,
                inner: oslog::OsLogger::new(OSLOG_SUBSYSTEM),
            }));
        }
    }
}

fn build_filter(rust_log: Option<&str>, default_filter: &str) -> env_filter::Filter {
    env_filter::Builder::new()
        .parse(rust_log.unwrap_or(default_filter))
        .build()
}

/// `oslog` filters by level only, so this applies the env_logger rules first.
struct Filtered<L: log::Log> {
    filter: env_filter::Filter,
    inner: L,
}

impl<L: log::Log> log::Log for Filtered<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.filter.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if self.filter.matches(record) {
            self.inner.log(record);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recording(Mutex<Vec<String>>);

    impl log::Log for Recording {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, record: &log::Record) {
            self.0.lock().unwrap().push(record.target().to_string());
        }
        fn flush(&self) {}
    }

    fn send(logger: &impl log::Log, target: &str) {
        logger.log(
            &log::Record::builder()
                .level(log::Level::Info)
                .target(target)
                .args(format_args!("message"))
                .build(),
        );
    }

    #[test]
    fn backend_chooses_oslog_only_for_the_exact_value() {
        assert_eq!(backend(None), Backend::Stderr);
        assert_eq!(backend(Some("oslog")), Backend::OsLog);
        assert_eq!(backend(Some("OSLOG")), Backend::Stderr);
        assert_eq!(backend(Some("stderr")), Backend::Stderr);
    }

    #[test]
    fn filtered_forwards_only_admitted_records() {
        let logger = Filtered {
            filter: build_filter(None, crate::DEFAULT_LOG_FILTER),
            inner: Recording::default(),
        };
        send(&logger, "chronicle_daemon");
        send(&logger, "hyper::client");
        assert_eq!(
            *logger.inner.0.lock().unwrap(),
            vec!["chronicle_daemon".to_string()]
        );
    }

    #[test]
    fn filtered_honors_a_rust_log_override() {
        let logger = Filtered {
            filter: build_filter(Some("warn"), crate::DEFAULT_LOG_FILTER),
            inner: Recording::default(),
        };
        send(&logger, "chronicle_daemon");
        send(&logger, "hyper::client");
        assert!(logger.inner.0.lock().unwrap().is_empty());
    }
}
