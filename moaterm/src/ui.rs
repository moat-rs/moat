use itertools::Itertools;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Stylize},
    widgets::{Block, BorderType, List, Paragraph, Widget},
};

use crate::{app::App, peer::selector::PeerSelector};

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
                Constraint::Min(self.peer_selector.min_width() as _),
                Constraint::Percentage(100),
            ])
            .split(block.inner(area));

        block.render(area, buf);
        self.peer_selector.render(rects[0], buf);
    }
}

impl Widget for &PeerSelector {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let block = Block::bordered()
            .title(" Peers ")
            .title_alignment(Alignment::Center)
            .border_type(BorderType::Plain);

        let peers = self.peers.iter().map(|peer| peer.to_string()).collect_vec();
        let list = List::new(peers);

        let inner = block.inner(area);

        block.render(area, buf);
        list.render(inner, buf);
    }
}
