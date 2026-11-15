//! Charts: flows as bars stacked by label, levels as lines.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph, Widget};

use crate::format;
use crate::series::Kind;
use crate::style::{color, DIM};
use crate::view::ChartView;

const EIGHTHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Draws `chart` inside `area`, a block's inner area.
pub fn draw(chart: &ChartView, area: Rect, buf: &mut Buffer) {
    if area.height < 3 || area.width < 12 {
        return;
    }
    let legend = Rect { height: 1, ..area };
    let plot = Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };
    legend_line(chart, area.width as usize).render(legend, buf);
    let empty = chart.lines.iter().all(|l| l.values.iter().all(|v| *v == 0));
    if empty {
        let y = plot.y + plot.height / 2;
        Paragraph::new(Line::from(Span::styled("nothing in this range", DIM)).centered()).render(
            Rect {
                y,
                height: 1,
                ..plot
            },
            buf,
        );
        return;
    }
    match chart.kind {
        Kind::Flow => bars(chart, plot, buf),
        Kind::Level => lines(chart, plot, buf),
    }
}

/// Each label with its total over the range (its last value for a level),
/// in `width`: closer together, then without the totals, then as many
/// labels as fit and how many more, rather than one cut.
fn legend_line(chart: &ChartView, width: usize) -> Line<'static> {
    let mut entries = Vec::new();
    for line in &chart.lines {
        let total = match chart.kind {
            Kind::Flow => line.values.iter().sum::<i64>(),
            Kind::Level => line.values.last().copied().unwrap_or(0),
        };
        if chart.kind == Kind::Flow && total == 0 {
            continue;
        }
        let total = format!(" {}", format::compact(total as f64, chart.unit));
        entries.push((color(line.color), line.label.clone(), total));
    }
    let length = |shown: usize, totals: bool, gap: usize| -> usize {
        let each = entries[..shown].iter().map(|(_, label, total)| {
            2 + label.chars().count() + if totals { total.chars().count() } else { 0 }
        });
        each.sum::<usize>() + gap * shown.saturating_sub(1)
    };
    let all = entries.len();
    let form = [(true, 3), (true, 2), (false, 3)]
        .into_iter()
        .find(|&(totals, gap)| length(all, totals, gap) <= width);
    let (totals, gap) = form.unwrap_or((false, 2));
    let more = |shown: usize| match all - shown {
        0 => String::new(),
        n => format!("{:gap$}+{n} more", ""),
    };
    let mut shown = all;
    while shown > 0 && length(shown, totals, gap) + more(shown).chars().count() > width {
        shown -= 1;
    }
    let mut spans = Vec::new();
    for (i, (color, label, total)) in entries.into_iter().take(shown).enumerate() {
        if i > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        spans.push(Span::styled("■ ", Style::new().fg(color)));
        spans.push(Span::raw(label));
        if totals {
            spans.push(Span::styled(total, DIM));
        }
    }
    spans.push(Span::styled(more(shown), DIM));
    Line::from(spans)
}

/// The axis: a label for the top, the middle and zero, and its width.
fn y_labels(max: f64, chart: &ChartView) -> (Vec<String>, u16) {
    let labels: Vec<String> = [max, max / 2.0, 0.0]
        .iter()
        .map(|v| {
            format::compact(*v, chart.unit)
                .trim_end_matches(" sat")
                .to_string()
        })
        .collect();
    let width = labels.iter().map(|l| l.chars().count()).max().unwrap_or(1) as u16 + 1;
    (labels, width)
}

/// Ticks under the first, middle and last buckets.
fn x_ticks(chart: &ChartView, plot: Rect, buf: &mut Buffer) {
    let bucket = chart.starts.get(1).map_or(86_400, |b| b - chart.starts[0]);
    let n = chart.starts.len();
    if n == 0 {
        return;
    }
    let y = plot.y + plot.height - 1;
    let marks = [(0, 0.0), (n / 2, 0.5), (n - 1, 1.0)];
    for (i, (at, frac)) in marks.into_iter().enumerate() {
        let text = format::tick(chart.starts[at], bucket);
        let len = text.chars().count() as u16;
        let x = match i {
            0 => plot.x,
            2 => (plot.x + plot.width).saturating_sub(len),
            _ => plot.x + ((plot.width as f64 * frac) as u16).saturating_sub(len / 2),
        };
        buf.set_string(x, y, text, DIM);
    }
}

fn bars(chart: &ChartView, area: Rect, buf: &mut Buffer) {
    let n = chart.starts.len();
    let totals: Vec<i64> = (0..n)
        .map(|i| chart.lines.iter().map(|l| l.values[i].max(0)).sum())
        .collect();
    let (labels, axis) = y_labels(*totals.iter().max().unwrap_or(&1) as f64, chart);
    let plot = Rect {
        x: area.x + axis,
        width: area.width.saturating_sub(axis),
        height: area.height - 1,
        ..area
    };
    let height = plot.height.max(1) as i64;
    let columns = (plot.width as usize).min(n).max(1);
    // Buckets per column, then the column's width and its bar's, a gap
    // between bars wherever there is room for one.
    let width = (plot.width as usize / columns).max(1);
    let bar = if width >= 2 { width - 1 } else { width };
    let plot = Rect {
        width: (columns * width) as u16,
        ..plot
    };
    let group = |c: usize| (c * n / columns)..((c + 1) * n / columns);
    let column_totals: Vec<i64> = (0..columns)
        .map(|c| group(c).map(|i| totals[i]).sum())
        .collect();
    let max = *column_totals.iter().max().unwrap_or(&1).max(&1);
    let (labels, _) = if columns < n {
        y_labels(max as f64, chart)
    } else {
        (labels, axis)
    };
    for (row, label) in [
        (0, &labels[0]),
        (height / 2, &labels[1]),
        (height - 1, &labels[2]),
    ] {
        let x = area.x + axis - 1 - label.chars().count() as u16;
        buf.set_string(x, plot.y + row as u16, label, DIM);
    }
    for c in 0..columns {
        // Eighths of the column's height each label stacks up to.
        let mut tops = Vec::new();
        let mut acc = 0;
        for line in &chart.lines {
            acc += group(c).map(|i| line.values[i].max(0)).sum::<i64>();
            tops.push((acc * height * 8 + max / 2) / max);
        }
        let x0 = plot.x + (c * width) as u16;
        for cell in 0..height {
            let (lo, hi) = (cell * 8, cell * 8 + 8);
            // The label covering most of the cell, and how much is covered.
            let mut best = (0, None);
            let mut below = 0;
            for (i, top) in tops.iter().enumerate() {
                let covered = (*top).min(hi) - below.max(lo);
                if covered > best.0 {
                    best = (covered, Some(i));
                }
                below = below.max(*top);
            }
            let filled = tops.last().copied().unwrap_or(0).min(hi) - lo;
            let (Some(label), true) = (best.1, filled > 0) else {
                continue;
            };
            let glyph = EIGHTHS[(filled.min(8) - 1) as usize];
            let y = plot.y + (height - 1 - cell) as u16;
            for dx in 0..bar as u16 {
                if let Some(cell) = buf.cell_mut((x0 + dx, y)) {
                    cell.set_symbol(glyph)
                        .set_fg(color(chart.lines[label].color));
                }
            }
        }
    }
    x_ticks(
        chart,
        Rect {
            x: plot.x,
            width: plot.width.max(24).min(area.width.saturating_sub(axis)),
            ..area
        },
        buf,
    );
}

fn lines(chart: &ChartView, area: Rect, buf: &mut Buffer) {
    let points: Vec<Vec<(f64, f64)>> = chart
        .lines
        .iter()
        .map(|l| {
            l.values
                .iter()
                .enumerate()
                .map(|(i, v)| (i as f64, *v as f64))
                .collect()
        })
        .collect();
    let all = chart.lines.iter().flat_map(|l| l.values.iter().copied());
    let (lo, hi) = all.fold((0i64, 1i64), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let top = hi as f64 * 1.05;
    let (labels, _) = y_labels(top, chart);
    let datasets = chart
        .lines
        .iter()
        .zip(&points)
        .map(|(line, data)| {
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::new().fg(color(line.color)))
                .data(data)
        })
        .collect();
    let bucket = chart.starts.get(1).map_or(86_400, |b| b - chart.starts[0]);
    let n = chart.starts.len().max(1);
    let ticks = [0, n / 2, n - 1].map(|i| {
        Span::styled(
            format::tick(chart.starts.get(i).copied().unwrap_or(0), bucket),
            DIM,
        )
    });
    let y_axis = labels.into_iter().rev().map(|l| Span::styled(l, DIM));
    Chart::new(datasets)
        .legend_position(None)
        .x_axis(
            Axis::default()
                .bounds([0.0, (n - 1).max(1) as f64])
                .labels(ticks)
                .style(Style::new().fg(Color::DarkGray)),
        )
        .y_axis(
            Axis::default()
                .bounds([lo.min(0) as f64, top])
                .labels(y_axis)
                .style(Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)),
        )
        .render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Line as Series;
    use crate::Unit;

    fn legend(width: usize) -> String {
        let line = |label: &str, color: usize, value: i64| Series {
            label: label.into(),
            color,
            values: vec![value],
        };
        let chart = ChartView {
            title: "Notes outstanding".into(),
            unit: Unit::Sat,
            kind: Kind::Level,
            starts: vec![0],
            lines: vec![
                line("holders", 0, 28_778),
                line("hub's own", 1, 0),
                line("behind locks", 2, 0),
            ],
        };
        let line = legend_line(&chart, width);
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_legend_says_less_rather_than_cut() {
        let full = "■ holders 28.8k sat   ■ hub's own 0 sat   ■ behind locks 0 sat";
        assert_eq!(legend(80), full);
        assert_eq!(legend(61), full.replace("   ", "  "));
        assert_eq!(legend(40), "■ holders   ■ hub's own   ■ behind locks");
        assert_eq!(legend(31), "■ holders  ■ hub's own  +1 more");
        assert_eq!(legend(30), "■ holders  +2 more");
    }
}
