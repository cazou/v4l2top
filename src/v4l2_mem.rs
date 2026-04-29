use anyhow::Result;
use regex::Regex;
use std::{
    collections::HashMap,
    fs::{File, read_dir},
    io::{BufRead, BufReader},
    path::PathBuf,
};

use crate::v4l2_stats::V4L2Stream;

#[derive(Debug, Clone)]
pub struct DMABuffer {
    pub label: String,
    pub size: usize,
}

fn parse_mem_file(
    mem_file_path: &PathBuf,
    mem: &mut HashMap<V4L2Stream, Vec<DMABuffer>>,
) -> Result<()> {
    let line_regex =
        Regex::new(r"(?<creator>[^ ]+) +(?<fd>\d+) +(?<pid>\d+) +(?<size>\d+) +(?<label>[^ ].*)")?;
    let file = File::open(mem_file_path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        if let Some(caps) = line_regex.captures(&line?) {
            let entry = mem
                .entry(V4L2Stream::new(caps["pid"].parse()?, caps["fd"].parse()?))
                .or_default();
            entry.push(DMABuffer {
                label: caps["label"].to_string(),
                size: caps["size"].parse()?,
            });
        }
    }
    Ok(())
}

///
/// Retrieve the memory usage of each media process through the v4l2 debugfs file.
///
pub fn v4l2_mem_get_usage() -> Result<HashMap<V4L2Stream, Vec<DMABuffer>>> {
    let mut ret = HashMap::new();
    let debugfs_path = PathBuf::from("/sys/kernel/debug/v4l2/");

    for dir in read_dir(debugfs_path)? {
        let mut dev_path = dir?.path();
        dev_path.push("mem");

        parse_mem_file(&dev_path, &mut ret)?;
    }

    Ok(ret)
}
