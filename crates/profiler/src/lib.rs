// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

//! Opt-in developer profiling landmarks for Sustain.
//!
//! Profiling is process-global because the landmarks cross crate boundaries
//! during startup. The macros branch before evaluating their formatting
//! arguments, so disabled landmarks are a cheap atomic load and nothing else.

use std::{
    fmt,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[cfg(test)]
use std::sync::Arc;

static ENABLED: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

enum Sink {
    Stderr,
    #[cfg(test)]
    Capture(Arc<Mutex<Vec<String>>>),
}

/// Enable profiler output for the current process.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether profiler landmarks are enabled for this process.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Emit one profiler record. Prefer the `profile!`, `profile_mark!`, and
/// `profile_startup!` macros at call sites so disabled formatting stays free.
#[doc(hidden)]
pub fn emit(arguments: fmt::Arguments<'_>) {
    if !enabled() {
        return;
    }
    emit_enabled(arguments);
}

fn emit_enabled(arguments: fmt::Arguments<'_>) {
    let sink = sink().lock().expect("profiler sink mutex is not poisoned");
    match &*sink {
        Sink::Stderr => eprintln!("[PROFILE] {arguments}"),
        #[cfg(test)]
        Sink::Capture(records) => records
            .lock()
            .expect("profiler capture mutex is not poisoned")
            .push(format!("[PROFILE] {arguments}")),
    }
}

fn sink() -> &'static Mutex<Sink> {
    SINK.get_or_init(|| Mutex::new(Sink::Stderr))
}

/// A monotonic clock for a related group of profiler landmarks.
#[derive(Clone, Copy, Debug)]
pub struct ProfileScope {
    started_at: Instant,
}

impl ProfileScope {
    /// Start a scope only when profiling is enabled.
    pub fn start() -> Option<Self> {
        enabled().then(|| Self {
            started_at: Instant::now(),
        })
    }

    #[doc(hidden)]
    pub fn elapsed_ms(self) -> f64 {
        self.started_at.elapsed().as_secs_f64() * 1000.0
    }
}

/// Startup profiling buffer used before Sustain knows whether this process
/// acquired the primary-instance lock. Records are captured only when the
/// parsed CLI requested profiling, then flushed after the lock is acquired.
pub struct StartupProfiler {
    started_at: Option<Instant>,
    buffered_records: Vec<String>,
    active: bool,
}

impl StartupProfiler {
    /// Create an enabled startup profiler from a parsed CLI flag.
    pub fn start_if(enabled: bool) -> Self {
        Self {
            started_at: enabled.then(Instant::now),
            buffered_records: Vec::new(),
            active: false,
        }
    }

    /// Whether this startup profiler should record landmarks.
    pub fn enabled(&self) -> bool {
        self.started_at.is_some()
    }

    /// Activate process-global profiling and flush records captured before the
    /// primary-instance lock was acquired.
    pub fn activate(&mut self) {
        if !self.enabled() || self.active {
            return;
        }
        enable();
        self.active = true;
        for record in self.buffered_records.drain(..) {
            emit_enabled(format_args!("{record}"));
        }
    }

    /// Record a startup landmark relative to the startup profiler's origin.
    #[doc(hidden)]
    pub fn record(&mut self, arguments: fmt::Arguments<'_>) {
        let Some(started_at) = self.started_at else {
            return;
        };
        let elapsed_ms = started_at.elapsed().as_secs_f64() * 1000.0;
        if self.active {
            emit(format_args!("{elapsed_ms:>8.1}ms {arguments}"));
        } else {
            self.buffered_records
                .push(format!("{elapsed_ms:>8.1}ms {arguments}"));
        }
    }
}

/// Emit a profiler record without elapsed time.
#[macro_export]
macro_rules! profile {
    ($($argument:tt)*) => {{
        if $crate::enabled() {
            $crate::emit(format_args!($($argument)*));
        }
    }};
}

/// Emit a profiler record relative to a `ProfileScope`.
#[macro_export]
macro_rules! profile_mark {
    ($scope:expr, $($argument:tt)*) => {{
        if let Some(scope) = ($scope).as_ref() {
            $crate::emit(format_args!(
                "{:>8.1}ms {}",
                scope.elapsed_ms(),
                format_args!($($argument)*)
            ));
        }
    }};
}

/// Emit or buffer a startup profiler record.
#[macro_export]
macro_rules! profile_startup {
    ($profiler:expr, $($argument:tt)*) => {{
        if ($profiler).enabled() {
            ($profiler).record(format_args!($($argument)*));
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    static ARGUMENT_EVALUATED: AtomicBool = AtomicBool::new(false);

    fn capture_records<T>(enabled: bool, run: impl FnOnce() -> T) -> (T, Vec<String>) {
        let _guard = TEST_LOCK
            .lock()
            .expect("profiler test mutex is not poisoned");
        let records = Arc::new(Mutex::new(Vec::new()));
        {
            let mut sink = sink().lock().expect("profiler sink mutex is not poisoned");
            *sink = Sink::Capture(records.clone());
        }
        ENABLED.store(enabled, Ordering::Relaxed);
        let result = run();
        ENABLED.store(false, Ordering::Relaxed);
        {
            let mut sink = sink().lock().expect("profiler sink mutex is not poisoned");
            *sink = Sink::Stderr;
        }
        let records = records
            .lock()
            .expect("profiler capture mutex is not poisoned")
            .clone();
        (result, records)
    }

    #[test]
    fn disabled_path_does_not_emit_records_through_profiler_sink() {
        ARGUMENT_EVALUATED.store(false, Ordering::Relaxed);
        let (_result, records) = capture_records(false, || {
            profile!("this should stay silent {}", mark_argument_evaluated());
        });

        assert!(records.is_empty());
        assert!(!ARGUMENT_EVALUATED.load(Ordering::Relaxed));
    }

    #[test]
    fn enabled_path_emits_records_with_profile_prefix() {
        let (_result, records) = capture_records(true, || {
            profile!("main() entered");
        });

        assert_eq!(records, vec!["[PROFILE] main() entered"]);
    }

    #[test]
    fn disabled_scope_does_not_start_a_clock() {
        ARGUMENT_EVALUATED.store(false, Ordering::Relaxed);
        let (_result, records) = capture_records(false, || {
            let scope = ProfileScope::start();
            assert!(scope.is_none());
            profile_mark!(scope, "{}", mark_argument_evaluated());
        });

        assert!(records.is_empty());
        assert!(!ARGUMENT_EVALUATED.load(Ordering::Relaxed));
    }

    #[test]
    fn startup_profiler_buffers_until_activated() {
        let (_result, records) = capture_records(false, || {
            let mut profiler = StartupProfiler::start_if(true);
            profile_startup!(profiler, "main() entered");
            assert!(records_snapshot().is_empty());
            profiler.activate();
        });

        assert_eq!(records.len(), 1);
        assert!(records[0].starts_with("[PROFILE]"));
        assert!(records[0].ends_with("main() entered"));
    }

    fn records_snapshot() -> Vec<String> {
        let sink = sink().lock().expect("profiler sink mutex is not poisoned");
        match &*sink {
            Sink::Capture(records) => records
                .lock()
                .expect("profiler capture mutex is not poisoned")
                .clone(),
            Sink::Stderr => Vec::new(),
        }
    }

    fn mark_argument_evaluated() -> &'static str {
        ARGUMENT_EVALUATED.store(true, Ordering::Relaxed);
        "evaluated"
    }
}
