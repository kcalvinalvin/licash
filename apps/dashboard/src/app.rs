//! The terminal's loop: keys, reads of the service's records off its
//! thread, and drawing.

use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::render;
use crate::view::{PanelView, View};
use crate::{Range, Report};

/// What the operator is looking at.
#[derive(Debug, Default)]
pub struct Ui {
    pub range: Range,
    pub page: usize,
    /// Which of the page's lists the arrows scroll.
    pub focus: usize,
    /// Rows scrolled past, per page and list.
    pub scroll: HashMap<(usize, usize), usize>,
    pub help: bool,
}

/// How the last read went.
pub struct Status<'a> {
    pub read: Duration,
    pub reading: bool,
    pub error: Option<&'a str>,
}

pub(crate) enum Action {
    Quit,
    Refresh,
    Range,
    Nothing,
}

impl Ui {
    pub fn new(range: Range) -> Self {
        Self {
            range,
            ..Self::default()
        }
    }

    pub fn offset(&self, list: usize) -> usize {
        self.scroll.get(&(self.page, list)).copied().unwrap_or(0)
    }

    pub(crate) fn press(&mut self, key: KeyEvent, view: &View) -> Action {
        let pages = view.pages.len().max(1);
        let lists = lists(view, self.page);
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if self.help {
            self.help = false;
            return Action::Nothing;
        }
        let step = |ui: &mut Ui, by: isize| {
            let rows = lists.get(ui.focus).copied().unwrap_or(0);
            let at = ui.scroll.entry((ui.page, ui.focus)).or_default();
            *at = (*at as isize + by).clamp(0, rows.saturating_sub(1) as isize) as usize;
        };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Char('r') => return Action::Refresh,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Right | KeyCode::Char('l') => self.go((self.page + 1) % pages),
            KeyCode::Left | KeyCode::Char('h') => self.go((self.page + pages - 1) % pages),
            KeyCode::Char(c @ '1'..='9') => {
                let page = c as usize - '1' as usize;
                if page < pages {
                    self.go(page);
                }
            }
            KeyCode::Char(']') | KeyCode::Char('+') | KeyCode::Char('=') => {
                return self.shift(1);
            }
            KeyCode::Char('[') | KeyCode::Char('-') => return self.shift(-1),
            KeyCode::Tab if !lists.is_empty() => self.focus = (self.focus + 1) % lists.len(),
            KeyCode::BackTab if !lists.is_empty() => {
                self.focus = (self.focus + lists.len() - 1) % lists.len()
            }
            KeyCode::Down | KeyCode::Char('j') => step(self, 1),
            KeyCode::Up | KeyCode::Char('k') => step(self, -1),
            KeyCode::PageDown => step(self, 10),
            KeyCode::PageUp => step(self, -10),
            KeyCode::Home | KeyCode::Char('g') => step(self, isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => step(self, isize::MAX / 2),
            _ => {}
        }
        Action::Nothing
    }

    fn go(&mut self, page: usize) {
        self.page = page;
        self.focus = 0;
    }

    fn shift(&mut self, by: isize) -> Action {
        let at = Range::ALL
            .iter()
            .position(|r| *r == self.range)
            .unwrap_or(2) as isize;
        let next = (at + by).clamp(0, Range::ALL.len() as isize - 1) as usize;
        if Range::ALL[next] == self.range {
            return Action::Nothing;
        }
        self.range = Range::ALL[next];
        self.scroll.clear();
        Action::Range
    }
}

/// The rows of each list on `page`, in the order the focus cycles them.
pub fn lists(view: &View, page: usize) -> Vec<usize> {
    let Some(page) = view.pages.get(page) else {
        return Vec::new();
    };
    page.rows
        .iter()
        .flatten()
        .filter_map(|p| match p {
            PanelView::Table(t) => Some(t.rows.len()),
            PanelView::Breakdown(b) => Some(b.rows.len()),
            _ => None,
        })
        .collect()
}

pub fn run<F>(report: Report, mut load: F, range: Range, refresh: Duration) -> anyhow::Result<()>
where
    F: FnMut() -> anyhow::Result<Report> + Send + 'static,
{
    let (send, reports) = mpsc::channel::<Result<Report, String>>();
    let (wake, woken) = mpsc::channel::<()>();
    thread::spawn(move || loop {
        match woken.recv_timeout(refresh) {
            Ok(()) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if send.send(load().map_err(|e| format!("{e:#}"))).is_err() {
            return;
        }
    });

    let mut terminal = ratatui::try_init()?;
    let mut ui = Ui::new(range);
    let mut report = report;
    let mut view = View::new(&report, ui.range);
    let mut read = Instant::now();
    let mut reading = false;
    let mut error: Option<String> = None;
    let result = (|| -> anyhow::Result<()> {
        loop {
            let status = Status {
                read: read.elapsed(),
                reading,
                error: error.as_deref(),
            };
            terminal.draw(|f| render::draw(f, &view, &ui, &status))?;
            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match ui.press(key, &view) {
                        Action::Quit => return Ok(()),
                        Action::Refresh => {
                            reading = true;
                            let _ = wake.send(());
                        }
                        Action::Range => view = View::new(&report, ui.range),
                        Action::Nothing => {}
                    }
                }
            }
            while let Ok(next) = reports.try_recv() {
                reading = false;
                match next {
                    Ok(next) => {
                        report = next;
                        view = View::new(&report, ui.range);
                        read = Instant::now();
                        error = None;
                    }
                    Err(e) => error = Some(e),
                }
            }
        }
    })();
    ratatui::try_restore()?;
    result
}
