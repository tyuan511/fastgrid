//! M0 spike: how does gpui-kit's DataTable cope with a huge result grid?
//!
//! Cells are generated on demand from (row, col), so memory reflects the widget only.
//!
//! cargo run --release -p fg-app --example grid_spike -- [rows] [cols] [--bench]
//!
//! `--bench` auto-scrolls and prints frame-time stats and RSS once per second.

use std::time::{Duration, Instant};

use gpui_kit::component::table::*;
use gpui_kit::*;

struct Grid {
    rows: usize,
    columns: Vec<Column>,
}

impl Grid {
    fn new(rows: usize, cols: usize) -> Self {
        let columns = (0..cols)
            .map(|c| Column::new(format!("c{c}"), format!("col_{c}")).width(px(110.)))
            .collect();
        Self { rows, columns }
    }

    fn cell(row: usize, col: usize) -> String {
        match col % 4 {
            0 => (row * 31 + col).to_string(),
            1 => format!("name_{}_{}", row, col),
            2 => format!("{:.3}", (row as f64) * 0.37 + col as f64),
            _ => format!("2026-10-{:02} 12:{:02}:00", row % 28 + 1, col % 60),
        }
    }
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        Self::cell(row_ix, col_ix)
    }
}

struct Root {
    table: Entity<TableState<Grid>>,
    bench: Option<Bench>,
}

struct Bench {
    last: Instant,
    window_start: Instant,
    frames: u32,
    worst: Duration,
    started: Instant,
    duration: Option<Duration>,
    total_frames: u64,
    slow_frames: u64,
    worst_all: Duration,
}

fn rss_mb() -> f64 {
    let pid = std::process::id().to_string();
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0)
}

impl Render for Root {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(b) = self.bench.as_mut() {
            let now = Instant::now();
            let dt = now - b.last;
            b.last = now;
            b.frames += 1;
            b.worst = b.worst.max(dt);
            b.worst_all = b.worst_all.max(dt);
            b.total_frames += 1;
            if dt > Duration::from_millis(100) {
                b.slow_frames += 1;
            }
            if let Some(limit) = b.duration
                && now - b.started >= limit
            {
                println!(
                    "SUMMARY avg fps {:.1}  worst frame {:.1} ms  frames >100ms: {}  frames: {}",
                    b.total_frames as f64 / (now - b.started).as_secs_f64(),
                    b.worst_all.as_secs_f64() * 1000.0,
                    b.slow_frames,
                    b.total_frames
                );
                cx.quit();
            }
            if now - b.window_start >= Duration::from_secs(1) {
                println!(
                    "fps {:>5.1}  worst frame {:>6.1} ms  rss {:>7.1} MB",
                    b.frames as f64 / (now - b.window_start).as_secs_f64(),
                    b.worst.as_secs_f64() * 1000.0,
                    rss_mb()
                );
                b.window_start = now;
                b.frames = 0;
                b.worst = Duration::ZERO;
            }
            window.request_animation_frame();
        }
        div()
            .size_full()
            .child(DataTable::new(&self.table).stripe(true).bordered(false))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bench = args.iter().any(|a| a == "--bench");
    let duration = args
        .iter()
        .position(|a| a == "--duration")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs);
    let nums: Vec<usize> = args
        .iter()
        .filter_map(|a| a.parse().ok())
        .filter(|n| Some(Duration::from_secs(*n as u64)) != duration || *n > 1000)
        .collect();
    let rows = nums.first().copied().unwrap_or(1_000_000);
    let cols = nums.get(1).copied().unwrap_or(200);

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.activate(true);
        let opened = gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
            let table = cx.new(|cx| TableState::new(Grid::new(rows, cols), window, cx));
            if bench {
                // Scroll down 3 rows every ~8ms, wrapping around.
                let t = table.downgrade();
                cx.spawn(async move |cx| {
                    let mut row = 0usize;
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(8))
                            .await;
                        row = (row + 3) % rows;
                        if t.update(cx, |s, cx| s.scroll_to_row(row, cx)).is_err() {
                            break;
                        }
                    }
                })
                .detach();
            }
            cx.new(|_| Root {
                table,
                bench: bench.then(|| Bench {
                    last: Instant::now(),
                    window_start: Instant::now(),
                    frames: 0,
                    worst: Duration::ZERO,
                    started: Instant::now(),
                    duration,
                    total_frames: 0,
                    slow_frames: 0,
                    worst_all: Duration::ZERO,
                }),
            })
        });
        if let Err(err) = opened {
            eprintln!("failed to open window: {err}");
            cx.quit();
        }
    });
}
