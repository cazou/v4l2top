use std::{
    collections::{HashMap, VecDeque},
    time::Instant,
};

use anyhow::{Result, bail};
use crossterm::style::style;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    symbols,
    text::Line,
    widgets::{
        Axis, Block, Cell, Chart, Dataset, GraphType, LegendPosition, LineGauge, Row, Table,
        TableState,
    },
};
use std::collections::HashSet;

#[warn(unused_imports)]
use cli_log::*;

use crate::v4l2_mem::{DMABuffer, v4l2_mem_get_usage};
use crate::v4l2_stats::{V4L2Stream, V4l2FdInfo, find_all_v4l2_fdinfo};

const NUM_BARS: usize = 12;
const BAR_COLS: usize = 2;
const BAR_ROWS: usize = NUM_BARS / BAR_COLS;

/// Compact byte formatting
pub fn format_bytes(bytes: u64, show_bytes: bool) -> String {
    if show_bytes {
        return format!("{} B", bytes);
    }

    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.1}G", b / GB)
    } else if b >= MB {
        format!("{:.1}M", b / MB)
    } else if b >= KB {
        format!("{:.0}K", b / KB)
    } else {
        format!("{b}B")
    }
}

/// Follows the hw usage of a given FD.
#[derive(Debug, Clone)]
struct CodecUsage {
    last_read: Option<Instant>,
    last_value_ns: Option<u64>,
    current_usage: Option<u8>,
}

impl CodecUsage {
    /// Update the usage based on the latest fdinfo.
    fn update(&mut self, info: &V4l2FdInfo) {
        let value_ns = info
            .fields
            .get("media-engine-decoder")
            .unwrap_or(&"0 ns".to_string())
            .trim_end_matches(" ns")
            .parse::<u64>()
            .ok();
        if let (Some(last_read), Some(last_value_ns), Some(value_ns)) =
            (self.last_read, self.last_value_ns, value_ns)
        {
            let elapsed = last_read.elapsed().as_secs_f64();
            if elapsed > 0.0 {
                let usage =
                    ((value_ns - last_value_ns) as f64 / (elapsed * 1_000_000_000.0)) * 100.0;
                self.current_usage = Some(usage.min(100.0) as u8);
                return;
            }
        }

        self.last_read = Some(info.timestamp);
        self.last_value_ns = value_ns;
    }
}

enum UsageRendererType {
    Chart,
    PerPid,
    //    PerType,
}

impl UsageRendererType {
    fn shift(&mut self) {
        match self {
            UsageRendererType::Chart => *self = UsageRendererType::PerPid,
            UsageRendererType::PerPid => *self = UsageRendererType::Chart,
        }
    }
}

/// Tracks sticky assignment of Stream to bar slots.
struct StreamBarRenderer {
    assignments: [(Option<V4L2Stream>, u8); NUM_BARS],
    slots: [Option<V4L2Stream>; NUM_BARS],
    usage: u8,
    selected: Option<V4L2Stream>,
}

impl StreamBarRenderer {
    fn new() -> Self {
        Self {
            assignments: [(None, 0u8); NUM_BARS],
            slots: [None; NUM_BARS],
            usage: 0,
            selected: None,
        }
    }

    #[allow(dead_code)]
    fn total_usage(&self) -> u8 {
        self.usage.max(100)
    }

    /// Update assignments given current mem_map. Returns ordered (slot_index, pid, usage) for
    /// occupied slots and None for empty ones.
    fn update(&mut self, info: &HashMap<V4L2Stream, StreamInfo>, selected: Option<V4L2Stream>) {
        self.selected = selected;

        // Remove PIDs that are no longer present.
        for slot in self.slots.iter_mut() {
            if let Some(stream) = *slot {
                if !info.contains_key(&stream) {
                    *slot = None;
                }
            }
        }

        self.usage = info
            .iter()
            .fold(0, |acc, (_, m)| acc + m.usage.current_usage.unwrap_or(0));

        // Sort Streams by usage descending; keep only top NUM_BARS.
        let mut ranked: Vec<(V4L2Stream, u8)> = info
            .iter()
            .filter_map(|(&p, b)| {
                if let Some(pct) = b.usage.current_usage {
                    Some((p, pct))
                } else {
                    None
                }
            })
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
        ranked.truncate(NUM_BARS);
        let top_pids: HashMap<V4L2Stream, u8> = ranked.iter().copied().collect();

        // Evict Streams that fell out of the max NUM_BARS.
        for slot in self.slots.iter_mut() {
            if let Some(pid) = *slot {
                if !top_pids.contains_key(&pid) {
                    *slot = None;
                }
            }
        }

        // Assign new Streams to free slots.
        let assigned: HashMap<V4L2Stream, usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|pid| (pid, i)))
            .collect();
        let mut free_slots: VecDeque<usize> =
            (0..NUM_BARS).filter(|i| self.slots[*i].is_none()).collect();

        for &(stream, _) in &ranked {
            if !assigned.contains_key(&stream) {
                if let Some(slot_idx) = free_slots.pop_front() {
                    self.slots[slot_idx] = Some(stream);
                }
            }
        }

        // Build result array.
        self.assignments = [(None, 0u8); NUM_BARS];
        for (i, slot) in self.slots.iter().enumerate() {
            self.assignments[i] = match slot {
                Some(stream) => (
                    Some(*stream),
                    info.get(stream).unwrap().usage.current_usage.unwrap_or(0),
                ),
                None => (None, 0),
            };
        }
    }

    pub fn render(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let row_constraints: Vec<Constraint> =
            (0..BAR_ROWS+1).map(|_| Constraint::Length(1)).collect();
        let col_constraints = (0..BAR_COLS).map(|_| Constraint::Percentage(100 / BAR_COLS as u16)).collect::<Vec<Constraint>>();

        let rows_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints(row_constraints)
            .split(Block::bordered().title("VPU Usage (per PID)").inner(area));
        frame.render_widget(Block::bordered().title("VPU Usage (per PID)"), area);

        for row in 0..BAR_ROWS {
            let cols_layout = Layout::default()
                .direction(Direction::Horizontal)
                .constraints(col_constraints.clone())
                .split(rows_layout[row]);

            for col in 0..BAR_COLS {
                let idx = row * BAR_COLS + col;
                let (stream_opt, pct) = self.assignments[idx];

                if let Some(stream) = stream_opt {
                    let color = match pct {
                        0..=50 => Color::Green,
                        51..=80 => Color::Yellow,
                        _ => Color::Red,
                    };
                    let style = if let Some(selected_stream) = self.selected {
                        if stream == selected_stream {
                            Style::default().bold()
                        } else {
                            Style::default()
                        }
                    } else {
                        Style::default()
                    };
                    frame.render_widget(
                        LineGauge::default()
                            .label(format!("{:>9}({:>6}): {pct:>3}%", stream.pid, stream.fd))
                            .ratio(pct as f64 / 100.0)
                            .filled_symbol("|")
                            .filled_style(color)
                            .style(style),
                        cols_layout[col],
                    );
                }
            }
        }

        let color = match self.usage {
            0..=50 => Color::Green,
            51..=80 => Color::Yellow,
            _ => Color::Red,
        };
        frame.render_widget(
            LineGauge::default()
                .label(format!("Total usage: {:>3}%", self.usage))
                .ratio(self.usage as f64 / 100.0)
                .filled_symbol("|")
                .filled_style(Style::default().fg(color)),
            rows_layout[BAR_ROWS],
        );
    }
}

struct StreamTableRenderer {
    table_state: TableState,
    show_bytes: bool,
    full_cmd: bool,
    keys: Vec<V4L2Stream>,
}

impl StreamTableRenderer {
    fn new() -> Self {
        Self {
            table_state: TableState::default().with_selected(0),
            show_bytes: false,
            full_cmd: false,
            keys: vec![],
        }
    }

    fn render(
        &mut self,
        frame: &mut Frame,
        area: ratatui::layout::Rect,
        infos: &HashMap<V4L2Stream, StreamInfo>,
    ) {
        let columns = ["Driver", "PID", "FD", "Clock rate", "Total Memory", "Command"];
        let mut widths = (0..columns.len()-1)
            .map(|_| Constraint::Percentage(60 / (columns.len() - 1) as u16))
            .collect::<Vec<Constraint>>();
        widths.push(Constraint::Percentage(40));

        let header = Row::new(columns)
            .style(Style::new().bold())
            .bottom_margin(1);

        self.keys = infos.keys().copied().collect::<Vec<V4L2Stream>>();
        self.keys
            .sort_by(|a, b| a.pid.cmp(&b.pid).then(a.fd.cmp(&b.fd)));

        let rows = self
            .keys
            .iter()
            .map(|stream| {
                let info = infos.get(stream).unwrap();
                Row::new([
                    Cell::from(
                        info.v4l2_info
                            .fields
                            .get("media-driver")
                            .unwrap_or(&"unknown".to_string())
                            .to_string(),
                    ),
                    Cell::from(stream.pid.to_string()),
                    Cell::from(stream.fd.to_string()),
                    Cell::from(
                        info.v4l2_info
                            .fields
                            .get("media-curfreq-decoder")
                            .unwrap_or(&"unknown".to_string())
                            .to_string(),
                    ),
                    Cell::from({
                        let total_mem = info.mem_usage.iter().fold(0, |mem, e| mem + e.size);
                        format_bytes(total_mem as u64, self.show_bytes)
                    }),
                    Cell::from(if self.full_cmd {
                        info.cmdline.clone()
                    } else {
                        info.comm.clone()
                    }),
                ])
            })
            .collect::<Vec<_>>();

        let footer = Row::new(vec![Cell::from(
            format!("Total V4L2 FDs: {}", infos.len(),),
        )]);

        let table = Table::new(rows, widths)
            .block(
                Block::bordered()
                    .title("V4L2 Codec Usage (press 'q' to quit)")
                    .title_alignment(ratatui::layout::Alignment::Center),
            )
            .header(header)
            .footer(footer)
            .column_spacing(1)
            .style(Color::White)
            .row_highlight_style(Style::new().reversed())
            .highlight_symbol(">> ");

        frame.render_stateful_widget(table, area, &mut self.table_state);
    }

    pub fn selected_stream(&self) -> Option<V4L2Stream> {
        if let Some(idx) = self.table_state.selected() {
            self.keys.get(idx).copied()
        } else {
            None
        }
    }

    fn render_mem_details(
        &self,
        frame: &mut Frame,
        area: ratatui::layout::Rect,
        infos: &HashMap<V4L2Stream, StreamInfo>,
    ) {
        let mem_details = Block::bordered()
            .title("Memory usage details")
            .title_alignment(ratatui::layout::Alignment::Center);

        let mem_rows = if let Some(stream) = self.selected_stream() {
            infos[&stream]
                .mem_usage
                .iter()
                .map(|e| {
                    Row::new([
                        e.label.to_string(),
                        format_bytes(e.size as u64, self.show_bytes),
                    ])
                })
                .collect()
        } else {
            vec![]
        };

        let mem_table = Table::new(
            mem_rows,
            [Constraint::Percentage(50), Constraint::Percentage(50)],
        )
        .block(mem_details);

        frame.render_widget(mem_table, area);
    }

    pub fn select_previous(&mut self) {
        self.table_state.select_previous();
    }

    pub fn select_next(&mut self) {
        self.table_state.select_next();
    }

    pub fn show_bytes_flip(&mut self) {
        self.show_bytes = !self.show_bytes;
    }
}

/// Palette of distinguishable colors for PID lines.
const COLOR_POOL: &[Color] = &[
    Color::Red,
    Color::Green,
    Color::Blue,
    Color::Yellow,
    Color::Cyan,
    Color::Magenta,
    Color::LightRed,
    Color::LightGreen,
    Color::LightBlue,
    Color::LightYellow,
    Color::LightCyan,
    Color::LightMagenta,
];

/// A single snapshot of per-V4L2Stream memory at a given tick.
#[derive(Clone)]
struct Snapshot {
    tick: u64,
    per_stream: HashMap<V4L2Stream, u8>,
}

/// Rolling time-series history of per-process codec memory usage.
///
/// Records one snapshot per tick and builds cumulative (stacked) datasets
/// suitable for rendering with ratatui's `Chart` widget.
struct UsageHistoryRenderer {
    snapshots: VecDeque<Snapshot>,
    max_points: usize,
    tick: u64,
    /// Color assigned to each PID: (color, pool_index).
    pid_colors: HashMap<V4L2Stream, (Color, usize)>,
    /// Pool indices available for reuse.
    free_colors: Vec<usize>,
    next_color: usize,
    /// Pre-computed chart data, rebuilt on every `record()`.
    /// Each entry: (pid, color, cumulative points).
    chart_data: Vec<(V4L2Stream, Color, Vec<(f64, f64)>)>,
    x_min: f64,
    x_max: f64,
}

impl UsageHistoryRenderer {
    fn new(max_points: usize) -> Self {
        Self {
            snapshots: VecDeque::with_capacity(max_points),
            max_points,
            tick: 0,
            pid_colors: HashMap::new(),
            free_colors: Vec::new(),
            next_color: 0,
            chart_data: Vec::new(),
            x_min: 0.0,
            x_max: 1.0,
        }
    }

    /// Record a new snapshot. `usage` maps Stream -> Usage.
    fn record(&mut self, usage: &HashMap<V4L2Stream, StreamInfo>) {
        // Recycle colors for Streams no longer visible in any snapshot.
        let active_streams: HashSet<V4L2Stream> = self
            .snapshots
            .iter()
            .flat_map(|s| s.per_stream.keys().copied())
            .chain(usage.keys().copied())
            .collect();

        let gone: Vec<V4L2Stream> = self
            .pid_colors
            .keys()
            .copied()
            .filter(|stream| !active_streams.contains(stream))
            .collect();
        for stream in gone {
            if let Some((_, idx)) = self.pid_colors.remove(&stream) {
                self.free_colors.push(idx);
            }
        }

        // Assign colors to new Streams.
        for &stream in usage.keys() {
            if !self.pid_colors.contains_key(&stream) {
                let idx = self.free_colors.pop().unwrap_or_else(|| {
                    let i = self.next_color;
                    self.next_color += 1;
                    i
                });
                self.pid_colors
                    .insert(stream, (COLOR_POOL[idx % COLOR_POOL.len()], idx));
            }
        }

        if self.snapshots.len() == self.max_points {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(Snapshot {
            tick: self.tick,
            per_stream: usage
                .iter()
                .map(|(&s, u)| (s, u.usage.current_usage.unwrap_or(0)))
                .collect(),
        });
        self.tick += 1;

        self.rebuild_chart_data();
    }

    /// Recompute cumulative stacked data from visible snapshots.
    fn rebuild_chart_data(&mut self) {
        self.chart_data.clear();

        if self.snapshots.is_empty() {
            return;
        }

        // Sorted streams order determines stacking order.
        let mut all_streams: HashSet<V4L2Stream> = self
            .snapshots
            .iter()
            .flat_map(|s| s.per_stream.iter().map(|(&stream, _)| stream.clone()))
            .into_iter()
            .collect();
        let mut all_streams: Vec<V4L2Stream> = all_streams.drain().collect();
        all_streams.sort_by(|a, b| a.cmp(b));

        self.x_max = self.snapshots.back().unwrap().tick as f64;
        // Fixed-width window: always show max_points ticks so data scrolls
        // instead of compressing as new points arrive.
        self.x_min = self.x_max - self.max_points as f64;
        if self.x_max <= self.x_min {
            self.x_max = self.x_min + 1.0;
        }

        let mut max_total: f64 = 0.0;

        for (layer, &stream) in all_streams.iter().enumerate() {
            let color = self
                .pid_colors
                .get(&stream)
                .map(|(c, _)| *c)
                .unwrap_or(Color::White);

            // Build step-function points: horizontal line at old value,
            // then vertical jump to new value at each tick.
            let mut points: Vec<(f64, f64)> = Vec::new();
            let mut prev_y: Option<f64> = None;

            for snap in &self.snapshots {
                let x = snap.tick as f64;
                let cumulative: f64 = all_streams[..=layer]
                    .iter()
                    .map(|s| *snap.per_stream.get(s).unwrap_or(&0) as f64)
                    .sum();
                if cumulative > max_total {
                    max_total = cumulative;
                }
                // Extend horizontal line at the previous value up to this tick,
                // then drop/rise vertically to the new value.
                if let Some(py) = prev_y {
                    if (py - cumulative).abs() > f64::EPSILON {
                        points.push((x, py));
                    }
                }
                points.push((x, cumulative));
                prev_y = Some(cumulative);
            }

            self.chart_data.push((stream, color, points));
        }
    }

    /// Build a ratatui `Chart` widget borrowing from `self`.
    fn render(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        if self.chart_data.is_empty() {
            frame.render_widget(
                Chart::new(vec![])
                    .block(Block::bordered().title("Codec Usage"))
                    .x_axis(Axis::default().bounds([0.0, 1.0]))
                    .y_axis(Axis::default().bounds([0.0, 1.0])),
                area,
            );
        }

        // Render top layer first so lower (earlier PID) layers paint on top.
        let datasets: Vec<Dataset<'_>> = self
            .chart_data
            .iter()
            .rev()
            .map(|(stream, color, points)| {
                Dataset::default()
                    .name(format!("{} {}", stream.pid, stream.fd))
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::default().fg(*color))
                    .data(points)
            })
            .collect();

        frame.render_widget(
            Chart::new(datasets)
                .block(Block::bordered().title("Codec Usage"))
                .x_axis(
                    Axis::default()
                        .bounds([self.x_min, self.x_max])
                        .style(Style::default().fg(Color::DarkGray)),
                )
                .y_axis(
                    Axis::default()
                        .bounds([0.0, 100.0])
                        .labels(vec![
                            Line::from("0 %"),
                            Line::from("50 %"),
                            Line::from("100 %"),
                        ])
                        .style(Style::default().fg(Color::DarkGray)),
                )
                .legend_position(Some(LegendPosition::Left)),
            area,
        );
    }
}

struct StreamInfo {
    v4l2_info: V4l2FdInfo,
    comm: String,
    cmdline: String,
    usage: CodecUsage,
    mem_usage: Vec<DMABuffer>,
}

pub struct TopRenderer {
    usage_renderer: UsageRendererType,
    usage_history: UsageHistoryRenderer,
    stream_bars: StreamBarRenderer,
    table_renderer: StreamTableRenderer,
    infos: HashMap<V4L2Stream, StreamInfo>,
}

impl TopRenderer {
    pub fn new() -> Self {
        Self {
            usage_history: UsageHistoryRenderer::new(300),
            usage_renderer: UsageRendererType::PerPid,
            stream_bars: StreamBarRenderer::new(),
            table_renderer: StreamTableRenderer::new(),
            infos: HashMap::new(),
        }
    }

    fn read_file_to_string(path: String) -> Result<String> {
        std::fs::read_to_string(path).map(|s| s.trim().replace("\0", " ").to_string()).map_err(|e| e.into())
    }

    fn update_data(&mut self) -> Result<()> {
        let infos = match find_all_v4l2_fdinfo(None) {
            Ok(infos) => infos,
            Err(e) => {
                bail!("Error fetching V4L2 stats: {e}");
            }
        };

        let mem_list = v4l2_mem_get_usage()?;

        // Remove entries for closed FDs and update usage for active ones.
        let to_remove = self
            .infos
            .keys()
            .filter(|stream| !infos.keys().any(|info| info == *stream))
            .cloned()
            .collect::<Vec<_>>();
        for stream in to_remove {
            self.infos.remove(&stream);
        }

        for (stream, info) in &infos {
            let stream_info = self.infos.entry(*stream).or_insert(StreamInfo {
                v4l2_info: info.clone(),
                comm: Self::read_file_to_string(format!("/proc/{}/comm", stream.pid)).unwrap_or("unknown".to_string()),
                cmdline: Self::read_file_to_string(format!("/proc/{}/cmdline", stream.pid)).unwrap_or("unknown".to_string()),
                usage: CodecUsage {
                    last_read: None,
                    last_value_ns: None,
                    current_usage: None,
                },
                mem_usage: vec![],
            });

            stream_info.usage.update(info);
            stream_info.mem_usage = mem_list.get(&stream).unwrap_or(&vec![]).clone();
        }

        Ok(())
    }

    pub fn render(&mut self, frame: &mut Frame) {
        if let Err(e) = self.update_data() {
            frame.render_widget(
                Block::bordered()
                    .title(e.to_string())
                    .title_alignment(ratatui::layout::Alignment::Center),
                frame.area(),
            );
        }

        // Split layout: memory chart on top, table on bottom
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9),
                Constraint::Min(12),
                Constraint::Length(1),
            ])
            .split(frame.area());

        match self.usage_renderer {
            UsageRendererType::Chart => {
                self.usage_history.record(&self.infos);
                self.usage_history.render(frame, chunks[0]);
            }
            UsageRendererType::PerPid => {
                self.stream_bars.update(&self.infos, self.table_renderer.selected_stream());
                self.stream_bars.render(frame, chunks[0]);
            }
        };

        let [layout_left, layout_right] =
            Layout::horizontal([Constraint::Percentage(75), Constraint::Percentage(25)])
                .areas(chunks[1]);

        self.table_renderer.render(frame, layout_left, &self.infos);
        self.table_renderer
            .render_mem_details(frame, layout_right, &self.infos);

        // Render shortcuts
        let line = Line::default().spans([
            "F2".bold(),
            "UsageType".black().on_light_green(),
            "F3".bold(),
            "Pretty/Byte".black().on_light_green(),
            "F4".bold(),
            "FullCmd".black().on_light_green(),
            "q".bold(),
            "Quit".black().on_light_green(),
        ]);

        frame.render_widget(line, chunks[2]);
    }

    pub fn shift_usage_renderer(&mut self) {
        self.usage_renderer.shift();
    }

    pub fn show_bytes_flip(&mut self) {
        self.table_renderer.show_bytes_flip();
    }

    pub fn full_cmd_flip(&mut self) {
        self.table_renderer.full_cmd = !self.table_renderer.full_cmd;
    }

    pub fn select_previous(&mut self) {
        self.table_renderer.select_previous();
    }

    pub fn select_next(&mut self) {
        self.table_renderer.select_next();
    }
}
