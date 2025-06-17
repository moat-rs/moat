use std::{
    borrow::Cow,
    cell::{Ref, RefCell},
    collections::VecDeque,
    fmt::Debug,
    ops::Deref,
    rc::Rc,
};

use crossterm::event::KeyEvent;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Widget},
};

use crate::{app::App, op::OpState};

#[derive(Debug)]
pub struct Window {}

impl Window {
    /// Constructs a new instance of [`Window`].
    pub fn new() -> Self {
        Self {}
    }
}

impl Widget for &Window {
    /// Renders the user interface widgets.
    ///
    // This is where you add new widgets.
    // See the following resources:
    // - https://docs.rs/ratatui/latest/ratatui/widgets/index.html
    // - https://github.com/ratatui/ratatui/tree/master/examples
    fn render(self, area: Rect, buf: &mut Buffer) {
        // let layout = Layout::default()
        //     .direction(Direction::Vertical)
        //     .constraints(&[Constraint::Percentage(100)])
        //     .split(area);

        let block = Block::bordered()
            .title(" moaterm ")
            .title_alignment(Alignment::Center)
            .border_type(BorderType::Plain);

        let inner = block.inner(area);
        block.render(area, buf);

        let layout = Layout::default()
            .direction(ratatui::layout::Direction::Horizontal)
            .constraints([Constraint::Min(30), Constraint::Percentage(70)].as_ref())
            .split(inner);

        let info = PeerSelector::new();
        info.render(layout[0], buf);

        // let text = format!(
        //     "This is a tui template.\n\
        //         Press `Esc`, `Ctrl-C` or `q` to stop running.\n\
        //         Press left and right to increment and decrement the counter respectively.\n\
        //         Counter: {}",
        //     self.counter
        // );

        // let paragraph = Paragraph::new(text)
        //     .block(block)
        //     .fg(Color::Cyan)
        //     .bg(Color::Black)
        //     .centered();

        // paragraph.render(area, buf);
    }
}

struct PeerSelector {
    state: OpState,
}

impl PeerSelector {
    pub fn new() -> Self {
        Self {
            state: OpState::default(),
        }
    }

    pub fn styled<'a, T, S>(&'a self, content: T, style: S) -> Span
    where
        T: Into<Cow<'a, str>>,
        S: Into<Style>,
    {
        match self.state {
            OpState::Active | OpState::Wake => Span::styled(content.into(), style.into()),
            OpState::Sleep => Span::raw(content.into()),
        }
    }
}

impl Widget for PeerSelector {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = Line::from(vec![
            " ".into(),
            Span::styled(" P", Color::Red),
            "eers".into(),
            " ".into(),
            "(browser with ".into(),
            Span::styled("h,j,k,l", Color::Yellow),
            "/".into(),
            Span::styled("/←,↓,↑,→", Color::Yellow),
            "; select with ".into(),
            Span::styled("<enter>", Color::Yellow),
            ")".into(),
            " ".into(),
        ]);
        let block = Block::bordered()
            .title(title)
            .title_alignment(Alignment::Center)
            .border_type(match self.state {
                OpState::Active => BorderType::Thick,
                OpState::Wake | OpState::Sleep => BorderType::Plain,
            });
        block.render(area, buf);
    }
}
