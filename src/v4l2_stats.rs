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
use nix::libc;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[warn(unused_imports)]
use cli_log::*;

const V4L2_DEV_MAJOR_ID: u64 = 81;

// --- Media ioctl structures (Linux UAPI) ---

#[repr(C)]
#[derive(Debug, Clone)]
struct MediaDeviceInfo {
    driver: [libc::c_char; 16],
    model: [libc::c_char; 32],
    serial: [libc::c_char; 40],
    bus_info: [libc::c_char; 32],
    media_version: libc::__u32,
    hw_revision: libc::__u32,
    driver_version: libc::__u32,
    reserved: [libc::__u32; 31],
}

impl Default for MediaDeviceInfo {
    fn default() -> Self {
        Self {
            driver: [0; 16],
            model: [0; 32],
            serial: [0; 40],
            bus_info: [0; 32],
            media_version: 0,
            hw_revision: 0,
            driver_version: 0,
            reserved: [0; 31],
        }
    }
}

impl MediaDeviceInfo {
    fn driver_str(&self) -> String {
        let bytes: Vec<u8> = self.driver.iter().map(|&c| c as u8).collect();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).to_string()
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct MediaV2IntfDevnode {
    major: libc::__u32,
    minor: libc::__u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct MediaV2Interface {
    id: libc::__u32,
    intf_type: libc::__u32,
    flags: libc::__u32,
    reserved: [libc::__u32; 9],
    devnode: MediaV2IntfDevnode,
}

#[repr(C)]
#[derive(Debug, Clone)]
struct MediaV2Topology {
    topology_version: libc::__u64,
    num_entities: libc::__u32,
    reserved1: libc::__u32,
    ptr_entities: libc::__u64,
    num_interfaces: libc::__u32,
    reserved2: libc::__u32,
    ptr_interfaces: libc::__u64,
    num_pads: libc::__u32,
    reserved3: libc::__u32,
    ptr_pads: libc::__u64,
    num_links: libc::__u32,
    reserved4: libc::__u32,
    ptr_links: libc::__u64,
}

impl Default for MediaV2Topology {
    fn default() -> Self {
        Self {
            topology_version: 0,
            num_entities: 0,
            reserved1: 0,
            ptr_entities: 0,
            num_interfaces: 0,
            reserved2: 0,
            ptr_interfaces: 0,
            num_pads: 0,
            reserved3: 0,
            ptr_pads: 0,
            num_links: 0,
            reserved4: 0,
            ptr_links: 0,
        }
    }
}

// MEDIA_IOC_DEVICE_INFO = _IOWR('|', 0x00, struct media_device_info)
nix::ioctl_readwrite!(media_ioc_device_info, b'|', 0x00, MediaDeviceInfo);

// MEDIA_IOC_G_TOPOLOGY = _IOWR('|', 0x04, struct media_v2_topology)
nix::ioctl_readwrite!(media_ioc_g_topology, b'|', 0x04, MediaV2Topology);

/// Resolve the fd symlink to get the target device path.
fn resolve_fd_target(pid: usize, fd: usize) -> Option<PathBuf> {
    let link = PathBuf::from(format!("/proc/{pid}/fd/{fd}"));
    fs::read_link(link).ok()
}

/// Get the (major, minor) of a device path.
fn dev_rdev(path: &Path) -> Option<(u32, u32)> {
    fs::metadata(path).ok().map(|m| {
        let rdev = m.rdev();
        (linux_major(rdev) as u32, linux_minor(rdev) as u32)
    })
}

/// Return the Linux device minor number from a raw `dev_t` value.
fn linux_minor(dev: u64) -> u64 {
    (dev & 0xff) | ((dev >> 12) & !0xff_u64)
}

/// Query a /dev/mediaX device. Retrieves only the interfaces from the
/// topology. If all interfaces with a devnode point to the same /dev/videoX
/// (i.e. it is a V4L2 stateless decoder), return that devnode's
/// (major, minor) mapped to the media driver name. Otherwise return empty.
fn query_media_device(media_path: &Path) -> Result<HashMap<(u32, u32), String>> {
    let file = File::open(media_path)?;
    let fd = file.as_raw_fd();

    // Get the driver name
    let mut dev_info = MediaDeviceInfo::default();
    unsafe { media_ioc_device_info(fd, &mut dev_info) }?;
    let driver_name = dev_info.driver_str();

    // First call: get interface count
    let mut topo = MediaV2Topology::default();
    unsafe { media_ioc_g_topology(fd, &mut topo) }?;

    let num_interfaces = topo.num_interfaces as usize;
    if num_interfaces == 0 || num_interfaces > 64 {
        return Ok(HashMap::new());
    }

    // Allocate on the stack and fill only interfaces
    let mut interfaces = [MediaV2Interface::default(); 64];

    topo = MediaV2Topology::default();
    topo.num_interfaces = num_interfaces as libc::__u32;
    topo.ptr_interfaces = interfaces.as_mut_ptr() as libc::__u64;

    unsafe { media_ioc_g_topology(fd, &mut topo) }?;

    // Collect unique devnodes from all interfaces
    let devnodes: HashSet<(u32, u32)> = interfaces[..num_interfaces]
        .iter()
        .filter(|i| i.devnode.major != 0 || i.devnode.minor != 0)
        .map(|i| (i.devnode.major, i.devnode.minor))
        .collect();

    // A stateless decoder has all interfaces pointing to the same /dev/videoX
    if devnodes.len() != 1 {
        return Ok(HashMap::new());
    }

    let devnode = devnodes.into_iter().next().unwrap();
    Ok(HashMap::from([(devnode, driver_name)]))
}

/// Enumerate all /dev/mediaX devices and build a map of
/// (major, minor) -> driver name for stateless decoder devices only.
pub fn build_media_topology() -> HashMap<(u32, u32), String> {
    let mut combined = HashMap::new();

    let Ok(entries) = fs::read_dir("/dev") else {
        return combined;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if !name_str.starts_with("media") {
            continue;
        }

        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_char_device() {
            continue;
        }

        if let Ok(map) = query_media_device(&path) {
            combined.extend(map);
        }
    }

    combined
}

/// Resolve the driver name for a given pid/fd by:
/// 1. Reading the symlink /proc/<pid>/fd/<fd> to get /dev/videoX
/// 2. Looking up (major, minor) in the pre-built media topology map
pub fn resolve_decoder_name(
    pid: usize,
    fd: usize,
    topology: &HashMap<(u32, u32), String>,
) -> Option<String> {
    let target = resolve_fd_target(pid, fd)?;
    let (major, minor) = dev_rdev(&target)?;
    topology.get(&(major, minor)).cloned()
}

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
    /// Decoder entity name from the media topology (if found).
    pub driver: String,
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
fn find_v4l2_fdinfo_for_pid(
    pid: usize,
    media_topo: &HashMap<(u32, u32), String>,
) -> Result<HashMap<V4L2Stream, V4l2FdInfo>> {
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
        let driver = resolve_decoder_name(pid, fd, media_topo)
            .unwrap_or_else(|| "unknown".to_string());

        results.insert(V4L2Stream::new(pid, fd), V4l2FdInfo { fields, timestamp, driver });
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
    let media_topo = build_media_topology();

    match pid {
        Some(pid) => find_v4l2_fdinfo_for_pid(pid, &media_topo),
        None => {
            let mut all = HashMap::new();

            for entry in fs::read_dir("/proc")? {
                let name = entry?.file_name();
                let name_str = name.to_string_lossy();

                if let Ok(pid) = name_str.parse::<usize>()
                    && let Ok(infos) = find_v4l2_fdinfo_for_pid(pid, &media_topo)
                {
                    all.extend(infos);
                }
            }

            Ok(all)
        }
    }
}
