//! Chooses the daemon's log backend: unified logging when
//! `CHRONICLE_LOG_BACKEND=oslog`, otherwise env_logger on stderr.

const LOG_BACKEND_ENV: &str = "CHRONICLE_LOG_BACKEND";
const OSLOG_SUBSYSTEM: &str = "dev.heuss.chronicle";
const FATAL_TARGET: &str = "chronicle_daemon";

static FATAL_FILTER: std::sync::OnceLock<env_filter::Filter> = std::sync::OnceLock::new();

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
    init_backend(
        backend(std::env::var(LOG_BACKEND_ENV).ok().as_deref()),
        std::env::var("RUST_LOG").ok().as_deref(),
        default_filter,
    );
}

fn init_backend(backend: Backend, rust_log: Option<&str>, default_filter: &str) {
    let _ = FATAL_FILTER.set(build_filter(rust_log, default_filter));
    match backend {
        Backend::Stderr => env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or(default_filter),
        )
        .init(),
        Backend::OsLog => {
            chronicle_audio::set_device_identity_in_logs(false);
            let filter = build_filter(rust_log, default_filter);
            // set_boxed_logger leaves the global maximum level at Off.
            log::set_max_level(filter.filter());
            let _ = log::set_boxed_logger(Box::new(Filtered {
                filter,
                inner: oslog::OsLogger::new(OSLOG_SUBSYSTEM),
            }));
            // Not the `log` facade: OsLogger formats under a lock, so a panic in
            // a Display impl would deadlock a hook that logged through it.
            let handle = oslog::OsLog::new(OSLOG_SUBSYSTEM, FATAL_TARGET);
            install_panic_hook(move |line| handle.with_level(oslog::Level::Fault, line));
        }
    }
}

/// Passes each panic to `sink`, then runs the previous hook. launchd discards
/// stderr, so on the oslog backend the sink's line is the panic's only record.
fn install_panic_hook(sink: impl Fn(&str) + Send + Sync + 'static) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        sink(&panic_line(info));
        previous(info);
    }));
}

fn panic_line(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("<unnamed>");
    let message = info.payload_as_str().unwrap_or("Box<dyn Any>");
    match info.location() {
        Some(location) => format!("panic on thread '{name}' at {location}: {message}"),
        None => format!("panic on thread '{name}': {message}"),
    }
}

/// Logs a message that ends the process. When the filter would drop it, the
/// message goes to stderr, so the process never exits silently.
pub fn fatal(message: &str) {
    let admitted = FATAL_FILTER.get().is_some_and(|filter| {
        filter.matches(
            &log::Record::builder()
                .level(log::Level::Error)
                .target(FATAL_TARGET)
                .args(format_args!("{message}"))
                .build(),
        )
    });
    if admitted {
        log::error!(target: FATAL_TARGET, "{message}");
    } else {
        eprintln!("{message}");
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

    /// Serializes the tests that replace the process-wide panic hook.
    static HOOK_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn oslog_backend_redacts_device_identity() {
        let _hook = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(chronicle_audio::device_identity_in_logs());
        init_backend(Backend::OsLog, None, "off");
        drop(std::panic::take_hook());
        assert!(!chronicle_audio::device_identity_in_logs());
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
    fn panic_hook_logs_message_and_location_then_runs_the_previous_hook() {
        static SINK: Mutex<Vec<String>> = Mutex::new(Vec::new());
        static PREVIOUS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let _hook = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::panic::set_hook(Box::new(|info| {
            PREVIOUS.lock().unwrap().push(panic_line(info));
        }));
        install_panic_hook(|line| SINK.lock().unwrap().push(line.to_string()));
        let panic_line_number = line!() + 1;
        let _ = std::panic::catch_unwind(|| panic!("hook probe {}", 42));
        drop(std::panic::take_hook());

        let expected_location = format!("{}:{panic_line_number}:", file!());
        let probe = |l: &String| l.contains("hook probe 42") && l.contains(&expected_location);
        let sink = SINK.lock().unwrap().clone();
        let previous = PREVIOUS.lock().unwrap().clone();
        assert!(sink.iter().any(probe), "sink: {sink:?}");
        assert!(previous.iter().any(probe), "previous: {previous:?}");
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
