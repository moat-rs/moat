use itertools::Itertools;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Stylize},
    widgets::{Block, BorderType, List, Paragraph, Widget},
};

use crate::{app::App, peer::table::PeerTable};

impl Widget for &App {
    /// Renders the user interface widgets.
    ///
    // This is where you add new widgets.
    // See the following resources:
    // - https://docs.rs/ratatui/latest/ratatui/widgets/index.html
    // - https://github.com/ratatui/ratatui/tree/master/examples
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::bordered()
            .title(" moaterm ")
            .title_alignment(Alignment::Center)
            .border_type(BorderType::Plain);

        let rects = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(&[
                Constraint::Min(self.peer_selector.max_peer_len() as _),
                Constraint::Percentage(100),
            ])
            .split(block.inner(area));

        block.render(area, buf);
        self.peer_selector.render(rects[0], buf);
    }
}
