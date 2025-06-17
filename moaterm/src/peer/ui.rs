use itertools::Itertools;
use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Rect},
    style::{Color, Modifier, Style, Stylize},
    widgets::{Block, BorderType, Row, Table, Widget},
};

use crate::peer::table::PeerTable;

impl PeerTable {
    fn header(&self) -> Row<'_> {
        Row::new(["peer", "status", "last seen"])
            .style(Style::new().bold())
            .bottom_margin(1)
    }
}

impl Widget for &PeerTable {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let block = Block::bordered()
            .title(" Peers ")
            .title_alignment(Alignment::Center)
            .border_type(BorderType::Plain);

        let peers = self.peers.iter().map(|peer| peer.to_string()).collect_vec();

        let header = self.header();

        let rows = vec![];

        let table = Table::new(
            rows,
            [
                Constraint::Min(self.max_peer_len() as _),
                Constraint::Min(6),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::new().on_black().bold())
        .column_highlight_style(Color::Gray)
        .cell_highlight_style(Style::new().reversed().yellow())
        .highlight_symbol(">");

        let inner = block.inner(area);

        block.render(area, buf);
        table.render(inner, buf);
    }
}
