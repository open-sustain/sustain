// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Safe Rust wrappers around the two Linux scheduling-priority syscalls
//! the background-job scheduler reaches for: `setpriority` (CPU nice
//! value) and `ioprio_set` (block-device I/O priority).
//!
//! Why a dedicated module:
//!
//! * `ioprio_set` has no userland wrapper in glibc, `nix`, or `rustix`
//!   — every caller has to reach it through `libc::syscall`. Once `libc`
//!   is a required dependency for that, `setpriority` lives in the same
//!   place rather than pulling in a second crate to wrap one function.
//! * The workspace bans `unsafe_code` everywhere else; the audited
//!   `#![allow(unsafe_code)]` at the top of this file is the only
//!   exception, and it covers a small, audited set of
//!   `unsafe { libc::… }` calls — the two priority syscalls plus the
//!   best-effort E-core affinity helper. The safe public API takes typed
//!   enums so callers cannot pass an out-of-range nice value or an
//!   invalid I/O priority class.
//! * On hybrid CPUs the scheduler also pins the *polite* presets
//!   (Innocuous/Balanced) to the kernel-reported efficiency cores via
//!   [`pin_current_thread_to_efficiency_cores_best_effort`], so
//!   background analysis stays off the performance cores that playback
//!   and the UI want. This is Intel-hybrid-only and a no-op everywhere
//!   else (including AMD).
//! * Niceness and ionice are per-thread on Linux: the scheduler spawns
//!   N worker threads, each calls [`apply_to_current_thread`] once at
//!   the top of its loop, and the kernel never sees a thread that
//!   forgot to lower itself. There is no "set priority for the whole
//!   process" code path.
//!
//! Resolution of `BackgroundResourceUsage` to a `(NiceLevel,
//! IoPriorityClass)` pair lives in [`priority_for`], the single
//! authority the rest of the runtime should consult.

#![allow(unsafe_code)]

use std::io;

use sustain_domain::BackgroundResourceUsage;

/// Discrete CPU-scheduling stops the background scheduler uses. The
/// variants map onto a small, hand-picked subset of the kernel's
/// -20..=19 nice range — we deliberately do not expose a full
/// `NiceLevel(i8)` newtype, because the scheduler only ever needs
/// three stops and a typed enum makes the call sites self-explaining.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NiceLevel {
    /// Standard scheduling priority (nice value 0). Used by the
    /// `Aggressive` preset.
    Default,
    /// Moderately deprioritised (nice value 10). Used by the
    /// `Balanced` preset so background workers yield to playback and
    /// UI threads but still make solid progress.
    Background,
    /// Maximally deprioritised (nice value 19). Used by the
    /// `Innocuous` preset; the worker only runs when nothing else
    /// wants the CPU.
    Idle,
}

impl NiceLevel {
    /// Numeric nice value passed to `setpriority`. Range is -20..=19;
    /// every variant here is in 0..=19 (we never request *more* than
    /// default priority).
    fn as_value(self) -> libc::c_int {
        match self {
            Self::Default => 0,
            Self::Background => 10,
            Self::Idle => 19,
        }
    }
}

/// Block-device I/O scheduling class understood by Linux's
/// `ioprio_set`. We expose the three values the scheduler actually
/// uses; the kernel's `RealTime` class is intentionally unreachable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoPriorityClass {
    /// Kernel default best-effort class with default data (4). Used by
    /// the `Aggressive` preset — equivalent to not calling
    /// `ioprio_set` at all but kept explicit so a worker that ran
    /// under a different preset earlier resets to default.
    BestEffort,
    /// Best-effort class at the high data value (7). Used by the
    /// `Balanced` preset: still serviced by the disk scheduler but
    /// behind every default-priority caller.
    BestEffortLow,
    /// Idle class: the disk scheduler only services this thread when
    /// the device has nothing else to do. Used by the `Innocuous`
    /// preset.
    Idle,
}

impl IoPriorityClass {
    /// Pack the class + data fields into the single `int` that
    /// `ioprio_set` expects.
    fn as_ioprio(self) -> libc::c_int {
        // <linux/ioprio.h>: IOPRIO_CLASS_SHIFT = 13, and the priority
        // value is (class << 13) | (data & 0x1fff).
        const SHIFT: libc::c_int = 13;
        const CLASS_BE: libc::c_int = 2;
        const CLASS_IDLE: libc::c_int = 3;
        match self {
            // Best-effort, default data (4 — same value the kernel
            // assigns to a process inheriting nothing).
            Self::BestEffort => (CLASS_BE << SHIFT) | 4,
            // Best-effort, lowest priority within the class (7).
            Self::BestEffortLow => (CLASS_BE << SHIFT) | 7,
            // Idle class ignores the data field but we pass 0 for
            // clarity.
            Self::Idle => CLASS_IDLE << SHIFT,
        }
    }
}

/// Failures from the priority syscalls. Carries the `errno` value as a
/// [`std::io::Error`] so callers can decide whether to log-and-continue
/// or escalate.
#[derive(Debug)]
pub enum PriorityError {
    /// `setpriority` returned -1.
    NiceFailed(io::Error),
    /// `ioprio_set` returned -1.
    IoFailed(io::Error),
}

impl std::fmt::Display for PriorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NiceFailed(error) => write!(f, "setpriority failed: {error}"),
            Self::IoFailed(error) => write!(f, "ioprio_set failed: {error}"),
        }
    }
}

impl std::error::Error for PriorityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NiceFailed(error) | Self::IoFailed(error) => Some(error),
        }
    }
}

/// Apply both the nice value and the I/O priority class to the calling
/// thread. Both syscalls are independent and report errors separately;
/// if the nice call fails we still attempt the I/O call (and report
/// the nice failure), because partially-lowered is better than
/// not-lowered-at-all when the scheduler is trying to keep playback
/// responsive.
pub fn apply_to_current_thread(
    nice: NiceLevel,
    io_class: IoPriorityClass,
) -> Result<(), PriorityError> {
    let nice_outcome = set_thread_nice(nice);
    let io_outcome = set_thread_io_priority(io_class);
    nice_outcome?;
    io_outcome
}

/// Resolve a [`BackgroundResourceUsage`] preset to its `(nice, io)`
/// pair. Single authority — every caller that needs to know what
/// "Balanced" means in scheduling terms should go through here so the
/// preset can be tuned without scattering magic numbers.
pub fn priority_for(usage: BackgroundResourceUsage) -> (NiceLevel, IoPriorityClass) {
    match usage {
        BackgroundResourceUsage::Innocuous => (NiceLevel::Idle, IoPriorityClass::Idle),
        BackgroundResourceUsage::Balanced => {
            (NiceLevel::Background, IoPriorityClass::BestEffortLow)
        }
        BackgroundResourceUsage::Aggressive => (NiceLevel::Default, IoPriorityClass::BestEffort),
    }
}

/// Logical processors this process may run on. Wraps
/// [`std::thread::available_parallelism`] so callers do not have to
/// handle the fallback; a kernel that cannot report the CPU count
/// (extremely rare on Linux) counts as a single processor.
pub fn available_processors() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Number of worker threads to spawn for the given preset on this
/// machine.
pub fn resolve_worker_count(usage: BackgroundResourceUsage) -> usize {
    usage.worker_count(available_processors())
}

/// Whether a preset's workers should prefer the machine's efficiency
/// cores. The polite presets (Innocuous, Balanced) belong on E-cores —
/// background analysis is exactly the latency-insensitive work the
/// efficiency cores exist for, and keeping it off the performance cores
/// means less heat and less contention with playback/UI. `Aggressive`
/// is "drain the queue", so it stays unpinned and free to use every
/// core.
pub fn prefers_efficiency_cores(usage: BackgroundResourceUsage) -> bool {
    match usage {
        BackgroundResourceUsage::Innocuous | BackgroundResourceUsage::Balanced => true,
        BackgroundResourceUsage::Aggressive => false,
    }
}

/// Sysfs file Linux exposes on Intel hybrid topologies listing the
/// logical CPUs backed by Atom (efficiency) cores, e.g. `16-23`.
const EFFICIENCY_CORE_SYSFS_PATH: &str = "/sys/devices/cpu_atom/cpus";

/// Best-effort pin of the calling thread to the machine's efficiency
/// (E-) cores, when the kernel exposes them (Intel hybrid topologies via
/// `EFFICIENCY_CORE_SYSFS_PATH`). Returns `Ok(true)` when the thread
/// was pinned, `Ok(false)` when there is nothing to pin to (no hybrid
/// topology, or none of the E-cores are in the thread's allowed set).
///
/// AMD parts (including the maintainer's Ryzen machines) and pre-hybrid
/// Intel parts do not expose `cpu_atom`, so this is a no-op there — the
/// thread keeps its default affinity and runs across all cores.
///
/// The E-core set is intersected with the thread's *current* affinity
/// mask (`sched_getaffinity`) before being applied, so this cooperates
/// with cgroups / cpusets / `taskset` rather than fighting them: a
/// process already confined to a set with no E-cores is left untouched
/// instead of being stranded on CPUs it may not use.
///
/// Like the nice/ionice calls, failure is non-fatal to the caller: the
/// worker simply runs without the pin. Errors are surfaced as
/// [`io::Error`] so the caller can decide whether to log them.
pub fn pin_current_thread_to_efficiency_cores_best_effort() -> io::Result<bool> {
    let listing = match std::fs::read_to_string(EFFICIENCY_CORE_SYSFS_PATH) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    let efficiency_cores = parse_cpu_list(&listing);
    if efficiency_cores.is_empty() {
        return Ok(false);
    }

    // SAFETY: `cpu_set_t` is a plain fixed-size bitmask with no
    // invariants beyond being initialized; `zeroed` + `CPU_ZERO` is the
    // documented way to clear it. `sched_getaffinity` is passed the
    // type's own size and a pointer to that initialized set.
    let mut allowed: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut allowed) };
    let rc =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut allowed) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }

    // Intersect the E-core set with what this thread is already allowed
    // to run on, so we never widen or fight an externally-imposed mask.
    let mut target: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    unsafe { libc::CPU_ZERO(&mut target) };
    let mut selected = 0_usize;
    for cpu in efficiency_cores {
        // SAFETY: `CPU_ISSET`/`CPU_SET` read/modify the initialized sets;
        // `cpu` is bounds-checked against `CPU_SETSIZE` first.
        if cpu < libc::CPU_SETSIZE as usize && unsafe { libc::CPU_ISSET(cpu, &allowed) } {
            unsafe { libc::CPU_SET(cpu, &mut target) };
            selected += 1;
        }
    }
    if selected == 0 {
        // The thread is confined to a set with no E-cores — leave it be.
        return Ok(false);
    }

    // SAFETY: `target` is an initialized `cpu_set_t` with at least one
    // bit set, passed with its own size to the calling thread (`pid 0`).
    let rc = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &target) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

/// Parse a Linux CPU-list string (`"16-23"`, `"0-3,8,10-12"`) into the
/// logical CPU indices it names. Malformed fragments are skipped rather
/// than failing the whole parse: this feeds a best-effort affinity hint,
/// so an unexpected kernel string should degrade to "pin to whatever
/// parsed" (or to a no-op), never to an error that bubbles up a worker.
fn parse_cpu_list(listing: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for fragment in listing
        .trim()
        .split(',')
        .filter(|fragment| !fragment.is_empty())
    {
        match fragment.split_once('-') {
            Some((start, end)) => {
                if let (Ok(start), Ok(end)) =
                    (start.trim().parse::<usize>(), end.trim().parse::<usize>())
                    && start <= end
                {
                    cpus.extend(start..=end);
                }
            }
            None => {
                if let Ok(cpu) = fragment.trim().parse::<usize>() {
                    cpus.push(cpu);
                }
            }
        }
    }
    cpus
}

fn set_thread_nice(nice: NiceLevel) -> Result<(), PriorityError> {
    // PRIO_PROCESS with `who = 0` targets the calling thread on Linux
    // (each task has its own task_struct.nice), exactly what the
    // scheduler wants: each worker lowers itself.
    let value = nice.as_value();
    // SAFETY: `setpriority` is a libc function with no preconditions
    // beyond well-defined argument values. `PRIO_PROCESS` is the
    // constant the libc crate vends; `0` for `who` is the documented
    // self-targeting value; `value` is in the kernel's -20..=19 range
    // by construction of `NiceLevel::as_value`. Unlike `getpriority`,
    // `setpriority` returns 0 on success and -1 on error — there is no
    // ambiguous "-1 is a valid result" case to disambiguate via errno.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, value) };
    if rc == -1 {
        return Err(PriorityError::NiceFailed(io::Error::last_os_error()));
    }
    Ok(())
}

fn set_thread_io_priority(class: IoPriorityClass) -> Result<(), PriorityError> {
    // `ioprio_set(which, who, ioprio)`:
    //   which = IOPRIO_WHO_PROCESS (1) targets a TID when who != 0,
    //   the calling thread when who == 0.
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    let ioprio = class.as_ioprio();
    // SAFETY: `SYS_ioprio_set` is the documented syscall number on
    // every Linux ABI we ship to (we do not target any other
    // kernel). The three arguments below are all C-compatible
    // integers; no pointers are passed across the boundary.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS as libc::c_long,
            0 as libc::c_long,
            ioprio as libc::c_long,
        )
    };
    if rc == -1 {
        return Err(PriorityError::IoFailed(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundResourceUsage, IoPriorityClass, NiceLevel, parse_cpu_list,
        prefers_efficiency_cores, priority_for, resolve_worker_count,
    };

    #[test]
    fn nice_values_cover_the_three_presets() {
        // Locked-in numeric values: changing these would silently
        // alter the scheduler's behavior on every shipping install.
        assert_eq!(NiceLevel::Default.as_value(), 0);
        assert_eq!(NiceLevel::Background.as_value(), 10);
        assert_eq!(NiceLevel::Idle.as_value(), 19);
    }

    #[test]
    fn ioprio_packs_class_and_data_correctly() {
        // CLASS_BE (2) << 13 | 4 = 16388
        assert_eq!(IoPriorityClass::BestEffort.as_ioprio(), (2 << 13) | 4);
        // CLASS_BE (2) << 13 | 7 = 16391
        assert_eq!(IoPriorityClass::BestEffortLow.as_ioprio(), (2 << 13) | 7);
        // CLASS_IDLE (3) << 13 = 24576
        assert_eq!(IoPriorityClass::Idle.as_ioprio(), 3 << 13);
    }

    #[test]
    fn priority_presets_map_correctly() {
        assert_eq!(
            priority_for(BackgroundResourceUsage::Innocuous),
            (NiceLevel::Idle, IoPriorityClass::Idle)
        );
        assert_eq!(
            priority_for(BackgroundResourceUsage::Balanced),
            (NiceLevel::Background, IoPriorityClass::BestEffortLow)
        );
        assert_eq!(
            priority_for(BackgroundResourceUsage::Aggressive),
            (NiceLevel::Default, IoPriorityClass::BestEffort)
        );
    }

    #[test]
    fn resolve_worker_count_uses_available_parallelism() {
        // `available_parallelism` is documented to never return 0,
        // and at minimum we want one worker; we cannot assert an exact
        // value (test runner doesn't know the machine), only that the
        // helper produces something sensible for every preset.
        let innocuous = resolve_worker_count(BackgroundResourceUsage::Innocuous);
        let balanced = resolve_worker_count(BackgroundResourceUsage::Balanced);
        let aggressive = resolve_worker_count(BackgroundResourceUsage::Aggressive);

        assert_eq!(innocuous, 1, "Innocuous always spawns exactly one worker");
        assert!(balanced >= 1, "Balanced must spawn at least one worker");
        assert!(
            aggressive >= balanced,
            "Aggressive ({aggressive}) must be >= Balanced ({balanced})",
        );
    }

    #[test]
    fn only_the_polite_presets_prefer_efficiency_cores() {
        assert!(prefers_efficiency_cores(BackgroundResourceUsage::Innocuous));
        assert!(prefers_efficiency_cores(BackgroundResourceUsage::Balanced));
        // Aggressive is "drain the queue" — it must stay free to use
        // every core, performance cores included.
        assert!(!prefers_efficiency_cores(
            BackgroundResourceUsage::Aggressive
        ));
    }

    #[test]
    fn parse_cpu_list_handles_ranges_singletons_and_mixes() {
        // The canonical hybrid form: a single contiguous range.
        assert_eq!(
            parse_cpu_list("16-23"),
            vec![16, 17, 18, 19, 20, 21, 22, 23]
        );
        // Mixed singletons and ranges, with a trailing newline as sysfs
        // emits.
        assert_eq!(
            parse_cpu_list("0-3,8,10-12\n"),
            vec![0, 1, 2, 3, 8, 10, 11, 12]
        );
        // A bare singleton.
        assert_eq!(parse_cpu_list("5"), vec![5]);
    }

    #[test]
    fn parse_cpu_list_skips_malformed_fragments() {
        // Empty input and pure noise parse to nothing rather than
        // erroring — the affinity hint is best-effort.
        assert!(parse_cpu_list("").is_empty());
        assert!(parse_cpu_list("\n").is_empty());
        assert!(parse_cpu_list("garbage").is_empty());
        // An inverted range is dropped; the valid neighbour survives.
        assert_eq!(parse_cpu_list("9-3,4"), vec![4]);
    }
}
