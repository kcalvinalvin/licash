//! Offline rendering for service fixtures, using the real terminal renderer.

use ratatui::{backend::TestBackend, Terminal};

use crate::{
    app::{Status, Ui},
    render,
    view::View,
    Range, Report,
};

/// Render one service page without a terminal or a running service.
pub fn screen(report: &Report, page: usize, width: u16, height: u16) -> String {
    let mut ui = Ui::new(Range::Month);
    ui.page = page;
    let status = Status {
        read: std::time::Duration::from_secs(3),
        reading: false,
        error: None,
    };
    draw(report, &ui, width, height, &status)
}

pub(crate) fn draw(report: &Report, ui: &Ui, width: u16, height: u16, status: &Status) -> String {
    let view = View::new(report, ui.range);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| render::draw(f, &view, ui, status))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    if std::env::var_os("DASHBOARD_SCREEN").is_some() {
        println!("{out}");
    }
    out
}
