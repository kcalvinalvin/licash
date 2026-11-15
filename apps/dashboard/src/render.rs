//! Drawing a view: a header with the range and the service's health, the
//! pages' tabs, the page's panels, and the keys.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Sparkline, Table, Widget,
};
use ratatui::Frame;

use crate::app::{Status, Ui};
use crate::format;
use crate::page::{Cell as Value, Tone};
use crate::style::{self, ACCENT, BOLD, BORDER, DIM, FOCUS};
use crate::view::{self, BreakdownView, CardView, Number, PanelView, Reads, TableView, View};
use crate::{chart, Range};

const CARD_HEIGHT: u16 = 5;

pub fn draw(f: &mut Frame, view: &View, ui: &Ui, status: &Status) {
    let [head, tabs, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    header(f, head, view, ui, status);
    f.render_widget(tab_line(view, ui), tabs);
    if let Some(page) = view.pages.get(ui.page) {
        let heights = page.rows.iter().map(|row| {
            if row.iter().all(|p| matches!(p, PanelView::Cards { .. })) {
                Constraint::Length(CARD_HEIGHT)
            } else {
                Constraint::Fill(1)
            }
        });
        let rows = Layout::vertical(heights).split(body);
        let mut list = 0;
        for (row, area) in page.rows.iter().zip(rows.iter()) {
            let cells = Layout::horizontal(row.iter().map(|_| Constraint::Fill(1))).split(*area);
            for (panel, area) in row.iter().zip(cells.iter()) {
                draw_panel(f, *area, panel, view, ui, &mut list);
            }
        }
    }
    footer(f, foot, status);
    if ui.help {
        help(f, body);
    }
}

fn header(f: &mut Frame, area: Rect, view: &View, ui: &Ui, status: &Status) {
    let left = Line::from(vec![
        Span::styled(" ◆ ", Style::new().fg(ACCENT)),
        Span::styled(view.title.clone(), BOLD),
        Span::styled(format!("  {}", view.subtitle), DIM),
    ]);
    let mut right = Vec::new();
    for range in Range::ALL {
        let text = format!(" {range} ");
        right.push(if range == ui.range {
            Span::styled(
                text,
                Style::new()
                    .fg(ratatui::style::Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(text, DIM)
        });
    }
    let (word, tone) = match view.health {
        Tone::Bad => ("failing", Tone::Bad),
        Tone::Warn => ("attention", Tone::Warn),
        _ => ("healthy", Tone::Good),
    };
    right.push(Span::styled(format!("   ● {word}"), style::tone(tone)));
    let read = if status.reading {
        "   reading…".to_string()
    } else {
        format!(
            "   read {} ago ",
            format::span(status.read.as_secs() as i64)
        )
    };
    right.push(Span::styled(read, DIM));
    f.render_widget(Paragraph::new(left), area);
    f.render_widget(Paragraph::new(Line::from(right).right_aligned()), area);
}

fn tab_line(view: &View, ui: &Ui) -> Paragraph<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (i, page) in view.pages.iter().enumerate() {
        let selected = i == ui.page;
        spans.push(Span::styled(format!(" {} ", i + 1), DIM));
        spans.push(if selected {
            Span::styled(
                page.title.clone(),
                Style::new()
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            Span::raw(page.title.clone())
        });
        spans.push(Span::raw("  "));
    }
    Paragraph::new(vec![Line::from(spans), Line::default()])
}

fn footer(f: &mut Frame, area: Rect, status: &Status) {
    let keys = " ←→ page  [ ] range  tab list  ↑↓ scroll  r read now  ? help  q quit ";
    if let Some(error) = status.error {
        let line = Line::from(vec![
            Span::styled(" ✗ ", style::tone(Tone::Bad)),
            Span::styled(
                format!("read failed, showing the last read: {error}"),
                style::tone(Tone::Bad),
            ),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else {
        f.render_widget(Paragraph::new(Line::styled(keys, DIM)), area);
    }
}

fn block(title: &str, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { FOCUS } else { BORDER }))
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                Style::new().fg(FOCUS)
            } else {
                BOLD
            },
        ))
        .padding(Padding::horizontal(1))
}

fn draw_panel(
    f: &mut Frame,
    area: Rect,
    panel: &PanelView,
    view: &View,
    ui: &Ui,
    list: &mut usize,
) {
    match panel {
        PanelView::Cards { cards } => {
            let areas = Layout::horizontal(cards.iter().map(|_| Constraint::Fill(1))).split(area);
            for (card, area) in cards.iter().zip(areas.iter()) {
                draw_card(f, *area, card, view);
            }
        }
        PanelView::Chart(c) => {
            let b = block(&c.title, false);
            let inner = b.inner(area);
            f.render_widget(b, area);
            chart::draw(c, inner, f.buffer_mut());
        }
        PanelView::Breakdown(b) => {
            let focused = ui.focus == *list;
            breakdown(f, area, b, focused, ui.offset(*list));
            *list += 1;
        }
        PanelView::Table(t) => {
            let focused = ui.focus == *list;
            table(f, area, t, view, focused, ui.offset(*list));
            *list += 1;
        }
        PanelView::Health { checks } => {
            let name = checks
                .iter()
                .map(|c| c.name.chars().count())
                .max()
                .unwrap_or(0);
            let lines: Vec<Line> = checks
                .iter()
                .map(|c| {
                    Line::from(vec![
                        Span::styled("● ", style::tone(c.tone)),
                        Span::styled(format!("{:<name$}", c.name), BOLD),
                        Span::styled(
                            format!("  {}", c.detail),
                            match c.tone {
                                Tone::Warn | Tone::Bad => style::tone(c.tone),
                                _ => DIM,
                            },
                        ),
                    ])
                })
                .collect();
            f.render_widget(Paragraph::new(lines).block(block("Health", false)), area);
        }
    }
}

fn value_text(value: Option<f64>, reads: Reads, width: u16) -> String {
    let Some(v) = value else {
        return "n/a".into();
    };
    match reads {
        Reads::Percent => format::percent(v),
        Reads::Unit(unit) => {
            let full = format::amount(v.round() as i64, unit);
            if full.chars().count() as u16 <= width {
                full
            } else {
                format::compact(v, unit)
            }
        }
    }
}

fn draw_card(f: &mut Frame, area: Rect, card: &CardView, view: &View) {
    let b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(BORDER))
        .title(Span::styled(format!(" {} ", card.title), DIM))
        .padding(Padding::horizontal(1));
    let inner = b.inner(area);
    f.render_widget(b, area);
    if inner.height == 0 {
        return;
    }
    let value = value_text(card.value, card.reads, inner.width);
    let value_style = match (card.value, card.tone) {
        (None, _) => DIM,
        (_, Tone::Normal) => BOLD,
        (_, tone) => style::tone(tone).add_modifier(Modifier::BOLD),
    };
    let second = match (card.value, card.previous, card.reads) {
        (Some(v), Some(p), Reads::Percent) => Line::from(vec![
            Span::styled(format::points(v - p), style::trend(v - p, card.better)),
            Span::styled(" vs before", DIM),
        ]),
        (Some(v), Some(p), Reads::Unit(_)) => {
            let change = view::change(v, p);
            Line::from(vec![
                Span::styled(format::change(change, v), style::trend(v - p, card.better)),
                Span::styled(format!(" vs prior {}", view.range), DIM),
            ])
        }
        _ => Line::styled(card.detail.clone(), DIM),
    };
    let [first, middle, last] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);
    f.render_widget(Paragraph::new(Span::styled(value, value_style)), first);
    f.render_widget(Paragraph::new(second), middle);
    if !card.trend.is_empty() {
        let lo = card.trend.iter().copied().min().unwrap_or(0).min(0);
        let data: Vec<u64> = card.trend.iter().map(|v| (v - lo) as u64).collect();
        let tail = data.len().saturating_sub(last.width as usize);
        Sparkline::default()
            .data(data[tail..].to_vec())
            .style(Style::new().fg(ACCENT))
            .render(last, f.buffer_mut());
    }
}

/// `n` in a column `width` wide: the first of its forms that fits, or the
/// shortest.
fn number(n: &Number, color: usize, width: usize) -> Line<'static> {
    let fit = |forms: Vec<String>| {
        let last = forms.len() - 1;
        let fits = forms.iter().position(|f| f.chars().count() <= width);
        forms[fits.unwrap_or(last)].clone()
    };
    match *n {
        Number::Value(v, unit) => Line::raw(fit(vec![
            format::amount(v, unit),
            format::compact(v as f64, unit),
            format::shortest(v as f64, unit),
        ]))
        .right_aligned(),
        Number::PerDay(v, unit) => {
            // Whole units from 10 a day on.
            let v = if v.abs() >= 10.0 { v.round() } else { v };
            Line::raw(fit(vec![
                format!("{}/d", format::compact(v, unit)),
                format!("{}/d", format::shortest(v, unit)),
            ]))
            .right_aligned()
        }
        Number::Share(None) => Line::styled("–", DIM).right_aligned(),
        Number::Share(Some(s)) => {
            // A bar of what is left beside the percentage, up to 8 wide.
            let cells = width.saturating_sub(7).min(8);
            let eighths = (s.clamp(0.0, 1.0) * (8 * cells) as f64).round() as usize;
            let bar =
                "█".repeat(eighths / 8) + ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"][eighths % 8];
            let bar = match cells {
                0 => String::new(),
                _ => format!("{bar:<cells$} "),
            };
            Line::from(vec![
                Span::styled(bar, Style::new().fg(style::color(color))),
                Span::raw(format!("{:>6}", format::percent(s))),
            ])
            .right_aligned()
        }
        Number::Change(c, v) => Line::styled(format::change(c, v), DIM).right_aligned(),
        Number::Days(None) => {
            let forms = vec!["not falling".to_owned(), "–".to_owned()];
            Line::styled(fit(forms), DIM).right_aligned()
        }
        Number::Days(Some(d)) => {
            let tone = if d < 7.0 {
                Tone::Bad
            } else if d < 30.0 {
                Tone::Warn
            } else {
                Tone::Good
            };
            Line::styled(format::days(d), style::tone(tone)).right_aligned()
        }
    }
}

fn breakdown(f: &mut Frame, area: Rect, b: &BreakdownView, focused: bool, offset: usize) {
    let title = format!("{} · {}", b.title, b.rows.len());
    let header = Row::new(
        std::iter::once(Cell::from(""))
            .chain(
                b.columns
                    .iter()
                    .map(|c| Cell::from(Line::raw(c.clone()).right_aligned())),
            )
            .collect::<Vec<_>>(),
    )
    .style(DIM);
    // The labels take twice what each column does, or what the longest
    // needs, up to near half; the columns share the rest, two apart.
    let n = b.columns.len() as u16;
    let inner = area.width.saturating_sub(2 + 2 * n);
    let longest = b
        .rows
        .iter()
        .map(|r| r.label.chars().count())
        .max()
        .unwrap_or(0);
    let share = 2 * inner / (n + 2);
    let labels = (longest as u16 + 2).clamp(share.min(inner * 9 / 20), share.max(inner * 9 / 20));
    let width = (inner.saturating_sub(labels) / n.max(1)) as usize;
    let rows = b.rows.iter().skip(offset).map(|row| {
        let label = Line::from(vec![
            Span::styled("■ ", Style::new().fg(style::color(row.color))),
            Span::raw(row.label.clone()),
        ]);
        Row::new(
            std::iter::once(Cell::from(label))
                .chain(
                    row.cells
                        .iter()
                        .map(|n| Cell::from(number(n, row.color, width))),
                )
                .collect::<Vec<_>>(),
        )
    });
    let widths = std::iter::once(Constraint::Length(labels))
        .chain(b.columns.iter().map(|_| Constraint::Fill(1)));
    let mut block = block(&title, focused);
    if b.rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(Line::styled("nothing in this range", DIM).centered()),
            Rect {
                y: inner.y + inner.height / 2,
                height: 1,
                ..inner
            },
        );
        return;
    }
    if offset > 0 {
        block = block.title_bottom(
            Line::styled(format!(" {}/{} ", offset + 1, b.rows.len()), DIM).right_aligned(),
        );
    }
    f.render_widget(
        Table::new(rows, widths)
            .header(header)
            .block(block)
            .column_spacing(2),
        area,
    );
}

fn table(f: &mut Frame, area: Rect, t: &TableView, view: &View, focused: bool, offset: usize) {
    let title = format!("{} · {}", t.title, t.rows.len());
    let mut block = block(&title, focused);
    if t.hidden > 0 {
        block = block.title_bottom(Line::styled(
            format!(" {} more before {} ", t.hidden, view.range),
            DIM,
        ));
    }
    if offset > 0 {
        block = block.title_bottom(
            Line::styled(format!(" {}/{} ", offset + 1, t.rows.len()), DIM).right_aligned(),
        );
    }
    if t.rows.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(Line::styled(t.empty.clone(), DIM).centered()),
            Rect {
                y: inner.y + inner.height / 2,
                height: 1,
                ..inner
            },
        );
        return;
    }
    let numeric = |c: &Value| matches!(c, Value::Sat(_) | Value::Int(_) | Value::Blocks(_));
    // Each column as wide as its widest text, the last taking the rest.
    let mut widths: Vec<usize> = t.columns.iter().map(|c| c.chars().count()).collect();
    let texts: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|r| {
            r.cells
                .iter()
                .map(|c| view::cell_text(c, view.now))
                .collect()
        })
        .collect();
    for row in &texts {
        for (i, text) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(text.chars().count()).min(48);
            }
        }
    }
    let constraints: Vec<Constraint> = widths
        .iter()
        .enumerate()
        .map(|(i, w)| {
            if i + 1 == widths.len() {
                Constraint::Fill(1)
            } else {
                Constraint::Length(*w as u16)
            }
        })
        .collect();
    // A column of numbers, though some rows have none, as a log's.
    let right = |i: usize| t.rows.iter().any(|r| r.cells.get(i).is_some_and(numeric));
    let header = Row::new(
        t.columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let line = Line::raw(c.clone());
                Cell::from(if right(i) { line.right_aligned() } else { line })
            })
            .collect::<Vec<_>>(),
    )
    .style(DIM);
    let rows = t.rows.iter().zip(&texts).skip(offset).map(|(row, texts)| {
        Row::new(
            texts
                .iter()
                .zip(&row.cells)
                .map(|(text, cell)| {
                    let line = Line::raw(text.clone());
                    Cell::from(if numeric(cell) {
                        line.right_aligned()
                    } else {
                        line
                    })
                })
                .collect::<Vec<_>>(),
        )
        .style(style::tone(row.tone))
    });
    f.render_widget(
        Table::new(rows, constraints)
            .header(header)
            .block(block)
            .column_spacing(2),
        area,
    );
}

fn help(f: &mut Frame, area: Rect) {
    let width = 64.min(area.width);
    let height = 16.min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let key = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("{k:>12}  "), Style::new().fg(ACCENT)),
            Span::raw(what.to_string()),
        ])
    };
    let lines = vec![
        key("← → h l 1-9", "switch page"),
        key("[ ] - +", "shorter or longer range"),
        key("tab ⇧tab", "move between lists"),
        key("↑ ↓ j k", "scroll the list"),
        key("pgup pgdn", "scroll faster, g G to the ends"),
        key("r", "read the records now"),
        key("q esc", "quit"),
        Line::default(),
        Line::styled("Totals compare with the same span just before.", DIM),
        Line::styled("Bars are per bucket; lines are levels read from", DIM),
        Line::styled("the records, as they stood at each bucket's end.", DIM),
        Line::styled("Times are UTC.", DIM),
    ];
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block("Keys", true)), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Unit;

    fn text(n: Number, width: usize) -> String {
        let line = number(&n, 0, width);
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn numbers_too_wide_for_their_column_read_shorter() {
        let big = || Number::Value(1_234_567, Unit::Sat);
        assert_eq!(text(big(), 13), "1,234,567 sat");
        assert_eq!(text(big(), 12), "1.2M sat");
        assert_eq!(text(Number::Value(5_720, Unit::Sat), 8), "5.7k sat");
        assert_eq!(text(Number::Value(5_720, Unit::Sat), 3), "5.7k sat");
        assert_eq!(text(Number::PerDay(5_720.9, Unit::Sat), 11), "5,721 sat/d");
        assert_eq!(text(Number::PerDay(5_720.9, Unit::Sat), 10), "5.7k sat/d");
        assert_eq!(text(Number::PerDay(2.25, Unit::Sat), 10), "2.2 sat/d");
        assert_eq!(text(Number::Days(None), 11), "not falling");
        assert_eq!(text(Number::Days(None), 8), "–");
        let share = text(Number::Share(Some(0.5)), 15);
        assert_eq!(
            (share.chars().count(), share.ends_with(" 50.0%")),
            (15, true)
        );
        assert_eq!(text(Number::Share(Some(0.5)), 7), " 50.0%");
    }
}
