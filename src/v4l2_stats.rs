/*
 * Copyright (c) 2025 Collabora.
 * MIT
 */

//! V4L2 fdinfo stats parser.
//!
//! Scans `/proc` to find file descriptors pointing to V4L2 character devices
//! and parses their associated `fdinfo` entries into key–value maps.
//!
//! # Example
//!
//! ```rust,no_run
//! use v4l2_stats::{find_all_v4l2_fdinfo, parse_fdinfo};
//!
//! // Parse a specific fdinfo file directly
//! let fields = parse_fdinfo("/proc/self/fdinfo/0").unwrap();
//! println!("pos: {}", fields.get("pos").unwrap_or(&"unknown".to_string()));
//!
//! // Scan all processes for V4L2 file descriptors
//! let infos = find_all_v4l2_fdinfo(None).unwrap();
//! for info in &infos {
//!     println!("pid={} fd={} buf-size={:?}", info.pid, info.fd, info.fields.get("buf-size"));
//! }
//!
//! // Scan a specific process
//! let infos = find_all_v4l2_fdinfo(Some(1234)).unwrap();
//! ```

use anyhow::Result;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

const V4L2_DEV_MAJOR_ID: u64 = 81;

/// A unique identifier for a V4L2 Stream (i.e. a file descriptor pointing to a V4L2 char device).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct V4L2Stream {
    pub pid: usize,
    pub fd: usize,
}

impl V4L2Stream {
    pub fn new(pid: usize, fd: usize) -> Self {
        Self { pid, fd }
    }
}

#[derive(Debug, Clone)]
pub struct V4l2FdInfo {
    /// All key–value pairs from the fdinfo file.
    pub fields: HashMap<String, String>,
    /// The time at which the file was read.
    pub timestamp: Instant,
}

/// Parse a single fdinfo file into a [`FdInfoMap`].
///
/// Lines that do not contain a `:` separator are silently skipped (e.g. the
/// standard `pos`, `flags`,... and lines that happen to have no value are still
/// split on `:` so they are included; truly malformed lines are dropped).
pub fn parse_fdinfo(path: impl AsRef<Path>) -> Result<HashMap<String, String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut map = HashMap::new();

    for line in reader.lines() {
        if let Some((key, value)) = line?.split_once(':') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    Ok(map)
}

/// Return the Linux device major number from a raw `dev_t` value.
fn linux_major(dev: u64) -> u64 {
    // From <sys/sysmacros.h>: major = bits [19:8] | bits [63:32]
    ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff_u64)
}

/// Return `true` if the symlink `fd_dir/<fd_name>` resolves to a V4L2 char device.
fn is_v4l2_fd(fd_dir: &Path, fd_name: &str) -> bool {
    // metadata() follows the symlink, giving us the target's attributes.
    fs::metadata(fd_dir.join(fd_name))
        .map(|m| m.file_type().is_char_device() && linux_major(m.rdev()) == V4L2_DEV_MAJOR_ID)
        .unwrap_or(false)
}

/// Find all V4L2 fdinfo entries for a single process.
fn find_v4l2_fdinfo_for_pid(pid: usize) -> Result<HashMap<V4L2Stream, V4l2FdInfo>> {
    let pid_dir = PathBuf::from(format!("/proc/{pid}"));
    let fd_dir = pid_dir.join("fd");
    let fdinfo_dir = pid_dir.join("fdinfo");

    let mut results = HashMap::new();

    for entry in fs::read_dir(&fdinfo_dir)? {
        let entry = entry?;
        let fd_name = entry.file_name();
        let fd_name_str = fd_name.to_string_lossy();

        // Only consider numeric entries (actual file descriptors).
        if !fd_name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        if !is_v4l2_fd(&fd_dir, &fd_name_str) {
            continue;
        }

        let fd: usize = fd_name_str.parse()?;

        let timestamp = Instant::now();
        let fields = parse_fdinfo(entry.path())?;

        results.insert(
            V4L2Stream::new(pid, fd as usize),
            V4l2FdInfo { fields, timestamp },
        );
    }

    Ok(results)
}

/// Find V4L2 fdinfo stats for the given PID, or all PIDs if `pid` is [`None`].
///
/// Returns a [`HashMap`] of ['V4L2Stream'] -> [`V4l2FdInfo`] entries, one per V4L2 file descriptor
/// found.  Errors from individual processes are silently ignored when scanning
/// all PIDs (the process may have exited mid-scan); errors from a specific PID
/// are propagated.
pub fn find_all_v4l2_fdinfo(pid: Option<usize>) -> Result<HashMap<V4L2Stream, V4l2FdInfo>> {
    match pid {
        Some(pid) => find_v4l2_fdinfo_for_pid(pid),
        None => {
            let mut all = HashMap::new();

            for entry in fs::read_dir("/proc")? {
                let name = entry?.file_name();
                let name_str = name.to_string_lossy();

                if let Ok(pid) = name_str.parse::<usize>()
                    && let Ok(infos) = find_v4l2_fdinfo_for_pid(pid)
                {
                    all.extend(infos);
                }
            }

            Ok(all)
        }
    }
}
