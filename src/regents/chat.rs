//! Chat view of a channel (Regents, ADR-0001): `regents channel view
//! <canal>`. A chat pane is a normal pane of the layout that runs this
//! command, so the layout, focus, mouse, popup and remote clients of herdr
//! treat it as any other pane. The view is an ordinary JSON API client: it
//! follows `channel.message_posted`, reads the history with `channel.read`
//! and posts the one-line draft with `channel.post`. The draft and the scroll
//! position are state of this process (ADR-0001 records this exception).
//!
//! The view draws only after a change, so an idle chat pane writes no bytes
//! to its PTY. It never exits on its own when the channel cannot be read: it
//! shows the error and tries again, so the pane keeps its place in the
//! layout.

use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::api::client::{parse_response_value, ApiClient};
use crate::api::schema::{
    ChannelAuthorKind, ChannelMessageInfo, ChannelPostParams, ChannelReadParams, EventData,
    EventEnvelope, EventsSubscribeParams, Method, Request, ResponseResult, Subscription,
    ThreadOpenParams,
};

/// Messages that one history read returns: the most recent ones at start, or
/// one page of older ones when the scroll reaches the top.
const PAGE_SIZE: u32 = 200;
/// Most messages that the view keeps in memory.
const MAX_MESSAGES: usize = 2_000;
/// Lines that one mouse wheel step scrolls.
const WHEEL_LINES: usize = 3;
/// Time between two tries to follow the channel again after the event stream
/// closes.
const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(1);
/// Time between two tries to read a channel that could not be read.
const RETRY_DELAY: Duration = Duration::from_secs(3);
/// Longest wait for one API request, so a stuck server cannot freeze the view.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for a terminal event before the loop checks the channel.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Marker before the name of a human author. Agents show `@name`, the system
/// shows `system`.
const HUMAN_MARKER: &str = "●";

/// What a key asks the chat view to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChatAction {
    None,
    /// Post this message body to the channel.
    Send(String),
    Quit,
}

/// State of the chat view: a bounded window of messages, the draft and the
/// scroll position. It does no I/O, so tests drive it directly.
#[derive(Debug, Clone)]
pub(crate) struct ChatView {
    channel: String,
    /// Messages in posting order, at most `MAX_MESSAGES`.
    messages: VecDeque<ChannelMessageInfo>,
    ids: HashSet<u64>,
    /// No message older than the first loaded one exists.
    older_exhausted: bool,
    /// A load of older messages dropped the newest ones from the window.
    newest_dropped: bool,
    draft: String,
    /// Rows above the newest line of the history. Zero follows new messages.
    scroll_from_bottom: usize,
    /// Size of the area of the next render, for page scrolling.
    viewport: (u16, u16),
    /// Problem to show in the header, for example a failed read.
    notice: Option<String>,
    /// Offset of the local time zone, for message times.
    offset: time::UtcOffset,
}

impl ChatView {
    pub(crate) fn new(channel: impl Into<String>, offset: time::UtcOffset) -> Self {
        Self {
            channel: channel.into(),
            messages: VecDeque::new(),
            ids: HashSet::new(),
            older_exhausted: false,
            newest_dropped: false,
            draft: String::new(),
            scroll_from_bottom: 0,
            viewport: (0, 0),
            notice: None,
            offset,
        }
    }

    /// Adds recent messages: a history read or a live message. A message
    /// that the view has already is ignored, so a read and the event stream
    /// can overlap. The oldest messages leave the window when it is full.
    pub(crate) fn add_recent(&mut self, messages: impl IntoIterator<Item = ChannelMessageInfo>) {
        if self.newest_dropped {
            // The window shows older history; the latest read comes back
            // when the scroll returns to the bottom.
            return;
        }
        for message in messages {
            if !self.ids.insert(message.id) {
                continue;
            }
            if self
                .messages
                .back()
                .is_none_or(|newest| newest.id < message.id)
            {
                self.messages.push_back(message);
            } else {
                let position = self.messages.partition_point(|known| known.id < message.id);
                self.messages.insert(position, message);
            }
        }
        while self.messages.len() > MAX_MESSAGES {
            if let Some(dropped) = self.messages.pop_front() {
                self.ids.remove(&dropped.id);
                self.older_exhausted = false;
            }
        }
    }

    /// Adds one page of messages older than the window. A page shorter than
    /// `page_size` means that the channel has no older message.
    pub(crate) fn add_older(&mut self, messages: Vec<ChannelMessageInfo>, page_size: u32) {
        if messages.len() < page_size as usize {
            self.older_exhausted = true;
        }
        for message in messages.into_iter().rev() {
            if self.ids.insert(message.id) {
                self.messages.push_front(message);
            }
        }
        while self.messages.len() > MAX_MESSAGES {
            if let Some(dropped) = self.messages.pop_back() {
                self.ids.remove(&dropped.id);
                self.newest_dropped = true;
            }
        }
    }

    /// Replaces the window with the latest messages of the channel.
    pub(crate) fn replace_with_latest(
        &mut self,
        messages: Vec<ChannelMessageInfo>,
        page_size: u32,
    ) {
        self.messages.clear();
        self.ids.clear();
        self.newest_dropped = false;
        self.older_exhausted = messages.len() < page_size as usize;
        self.add_recent(messages);
    }

    /// The message ID to read older messages before, when the scroll is at
    /// the top of the window and the channel may have older messages.
    pub(crate) fn wants_older(&self) -> Option<u64> {
        if self.older_exhausted
            || self.viewport.1 == 0
            || self.scroll_from_bottom < self.max_scroll()
        {
            return None;
        }
        self.messages.front().map(|oldest| oldest.id)
    }

    /// True when the scroll is back at the bottom of a window that lost its
    /// newest messages.
    pub(crate) fn wants_latest(&self) -> bool {
        self.newest_dropped && self.scroll_from_bottom == 0
    }

    pub(crate) fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub(crate) fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(crate) fn set_viewport(&mut self, width: u16, height: u16) {
        self.viewport = (width, height);
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.max_scroll());
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> ChatAction {
        if key.kind == KeyEventKind::Release {
            return ChatAction::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.draft.is_empty() {
                    return ChatAction::Quit;
                }
                self.draft.clear();
            }
            KeyCode::Char('d') if ctrl && self.draft.is_empty() => return ChatAction::Quit,
            KeyCode::Char(_) if ctrl => {}
            KeyCode::Char(ch) => self.draft.push(ch),
            KeyCode::Backspace => {
                self.draft.pop();
            }
            KeyCode::Esc => self.draft.clear(),
            KeyCode::Enter => {
                if self.draft.trim().is_empty() {
                    return ChatAction::None;
                }
                self.scroll_from_bottom = 0;
                return ChatAction::Send(std::mem::take(&mut self.draft));
            }
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1),
            KeyCode::PageUp => self.scroll_up(self.page()),
            KeyCode::PageDown => self.scroll_down(self.page()),
            KeyCode::Home => self.scroll_up(usize::MAX),
            KeyCode::End => self.scroll_from_bottom = 0,
            _ => {}
        }
        ChatAction::None
    }

    /// Scrolls on a mouse wheel step. Returns false for other mouse events,
    /// which change nothing.
    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_down(WHEEL_LINES),
            _ => return false,
        }
        true
    }

    /// Puts back a draft whose post failed, so the user can send it again.
    pub(crate) fn restore_draft(&mut self, body: String) {
        if self.draft.is_empty() {
            self.draft = body;
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        self.scroll_from_bottom = self
            .scroll_from_bottom
            .saturating_add(lines)
            .min(self.max_scroll());
    }

    fn scroll_down(&mut self, lines: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(lines);
    }

    fn page(&self) -> usize {
        usize::from(history_height(self.viewport.1).saturating_sub(1).max(1))
    }

    fn max_scroll(&self) -> usize {
        let (width, height) = self.viewport;
        self.history_paragraph()
            .line_count(width)
            .saturating_sub(usize::from(history_height(height)))
    }

    fn history_paragraph(&self) -> Paragraph<'_> {
        if self.messages.is_empty() {
            return Paragraph::new(Line::styled(
                format!("no messages in #{} yet", self.channel),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        Paragraph::new(Text::from(self.history_lines().0)).wrap(Wrap { trim: false })
    }

    /// The thread whose link is at `(column, row)` of `area`: a click on the
    /// first line of an agent message opens the thread that posted it.
    pub(crate) fn thread_at(&self, area: Rect, column: u16, row: u16) -> Option<String> {
        let history = history_area(area);
        let inside = column >= history.x
            && column < history.right()
            && row >= history.y
            && row < history.bottom();
        if !inside || self.messages.is_empty() {
            return None;
        }
        let (lines, links) = self.history_lines();
        let wrapped = |line: &Line<'_>| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(history.width)
        };
        let total: usize = lines.iter().map(wrapped).sum();
        let target = history_top(total, history.height, self.scroll_from_bottom)
            + usize::from(row - history.y);
        let mut top = 0;
        for (index, line) in lines.iter().enumerate() {
            let bottom = top + wrapped(line);
            if target < bottom {
                // Only the first row of the message is the link.
                return (target == top)
                    .then(|| links.iter().find(|(line, _)| *line == index))
                    .flatten()
                    .map(|(_, thread)| thread.clone());
            }
            top = bottom;
        }
        None
    }

    /// The history lines, and the index of the first line of each agent
    /// message that links to its thread.
    fn history_lines(&self) -> (Vec<Line<'_>>, Vec<(usize, String)>) {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut links = Vec::new();
        let mut lines = Vec::new();
        let mut last_date = None;
        for message in &self.messages {
            let local = message_time(&message.created_at).map(|time| time.to_offset(self.offset));
            if let Some(local) = local {
                if last_date != Some(local.date()) {
                    last_date = Some(local.date());
                    lines.push(Line::styled(
                        format!(
                            "── {:04}-{:02}-{:02} ──",
                            local.year(),
                            u8::from(local.month()),
                            local.day()
                        ),
                        dim,
                    ));
                }
            }
            let mut body = message.body.lines();
            let first = body.next().unwrap_or_default();
            let mut spans = Vec::with_capacity(4);
            if let Some(local) = local {
                spans.push(Span::styled(
                    format!("{:02}:{:02} ", local.hour(), local.minute()),
                    dim,
                ));
            }
            spans.push(author_span(message));
            if let Some(thread) = message
                .origin_thread_id
                .as_ref()
                .filter(|_| message.author.kind == ChannelAuthorKind::Agent)
            {
                links.push((lines.len(), thread.clone()));
                spans.push(Span::styled(
                    format!(" [{thread}]"),
                    dim.add_modifier(Modifier::UNDERLINED),
                ));
            }
            spans.push(Span::raw(": "));
            spans.push(Span::raw(first));
            lines.push(Line::from(spans));
            lines.extend(body.map(|line| Line::raw(format!("  {line}"))));
        }
        (lines, links)
    }

    /// Draws the view: a header with the channel name, the history, a
    /// separator and the input line. It changes no state.
    pub(crate) fn render(&self, area: Rect, buffer: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let mut header = vec![Span::styled(
            format!(" #{}", self.channel),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if let Some(notice) = &self.notice {
            header.push(Span::styled(
                format!("  {notice}"),
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        Paragraph::new(Line::from(header)).render(Rect { height: 1, ..area }, buffer);

        let history = history_area(area);
        let paragraph = self.history_paragraph();
        let total = paragraph.line_count(history.width);
        let top = history_top(total, history.height, self.scroll_from_bottom);
        paragraph
            .scroll((u16::try_from(top).unwrap_or(u16::MAX), 0))
            .render(history, buffer);

        if area.height < 3 {
            return;
        }
        let separator_y = area.bottom().saturating_sub(2);
        Paragraph::new(Line::styled(
            "─".repeat(usize::from(area.width)),
            Style::default().add_modifier(Modifier::DIM),
        ))
        .render(
            Rect {
                y: separator_y,
                height: 1,
                ..area
            },
            buffer,
        );
        let input = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        // Keep the end of a long draft visible.
        let visible = usize::from(area.width.saturating_sub(3));
        let skip = self.draft.chars().count().saturating_sub(visible);
        let shown: String = self.draft.chars().skip(skip).collect();
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(shown),
        ]))
        .render(input, buffer);
    }

    /// Screen position of the input cursor in `area`.
    pub(crate) fn cursor(&self, area: Rect) -> (u16, u16) {
        let visible = usize::from(area.width.saturating_sub(3));
        let shown = self.draft.chars().count().min(visible);
        (
            area.x
                .saturating_add(2)
                .saturating_add(u16::try_from(shown).unwrap_or(u16::MAX)),
            area.bottom().saturating_sub(1),
        )
    }
}

/// Rows of the history: the area minus the header, the separator and the
/// input line.
fn history_height(height: u16) -> u16 {
    height.saturating_sub(3)
}

fn history_area(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: history_height(area.height),
        ..area
    }
}

/// First shown line of a history of `total` wrapped lines.
fn history_top(total: usize, height: u16, scroll_from_bottom: usize) -> usize {
    let hidden = total.saturating_sub(usize::from(height));
    hidden.saturating_sub(scroll_from_bottom.min(hidden))
}

fn author_span(message: &ChannelMessageInfo) -> Span<'static> {
    let style = Style::default().add_modifier(Modifier::BOLD);
    match message.author.kind {
        ChannelAuthorKind::Agent => Span::styled(format!("@{}", message.author.id), style),
        ChannelAuthorKind::System => Span::styled("system", style.add_modifier(Modifier::ITALIC)),
        ChannelAuthorKind::Human => {
            Span::styled(format!("{HUMAN_MARKER} {}", message.author.id), style)
        }
        ChannelAuthorKind::Unknown => Span::styled(message.author.id.clone(), style),
    }
}

/// The instant of a message: `created_at` is RFC 3339 in UTC, in whole
/// seconds (`2026-11-27T18:04:05Z`).
fn message_time(created_at: &str) -> Option<time::OffsetDateTime> {
    let bytes = created_at.as_bytes();
    if bytes.len() != 20 || bytes[10] != b'T' || bytes[19] != b'Z' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| created_at.get(range)?.parse::<u16>().ok();
    let month = time::Month::try_from(u8::try_from(number(5..7)?).ok()?).ok()?;
    let date = time::Date::from_calendar_date(
        i32::from(number(0..4)?),
        month,
        u8::try_from(number(8..10)?).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms(
        u8::try_from(number(11..13)?).ok()?,
        u8::try_from(number(14..16)?).ok()?,
        u8::try_from(number(17..19)?).ok()?,
    )
    .ok()?;
    Some(time::PrimitiveDateTime::new(date, time).assume_utc())
}

/// Offset of the local time zone now. Call it before the view starts other
/// threads.
fn local_offset() -> time::UtcOffset {
    let Some(local) = crate::platform::local_datetime() else {
        return time::UtcOffset::UTC;
    };
    let seconds =
        local.assume_utc().unix_timestamp() - time::OffsetDateTime::now_utc().unix_timestamp();
    // Round to whole minutes: the two clock reads are not at the same instant.
    let minutes = (seconds + 30).div_euclid(60);
    i32::try_from(minutes * 60)
        .ok()
        .and_then(|seconds| time::UtcOffset::from_whole_seconds(seconds).ok())
        .unwrap_or(time::UtcOffset::UTC)
}

/// The channel as the view reads and writes it.
pub(crate) trait ChatSource {
    /// The public workspace ID and the most recent `limit` messages before
    /// `before` (all recent ones when absent), in posting order.
    fn read(
        &self,
        before: Option<u64>,
        limit: u32,
    ) -> Result<(String, Vec<ChannelMessageInfo>), String>;
    fn post(&self, body: String) -> Result<ChannelMessageInfo, String>;
    /// Shows the thread `thread_id` of the channel's workspace as a popup.
    fn open_thread(&self, thread_id: &str) -> Result<(), String>;
    /// The channel has a new name; later reads and posts use it.
    fn rename(&mut self, _channel: &str) {}
}

/// Input of the view loop besides the terminal events.
pub(crate) enum Update {
    Posted {
        workspace_id: String,
        message: ChannelMessageInfo,
    },
    /// A channel has a new name (`channel.renamed`).
    Renamed {
        workspace_id: String,
        from: String,
        channel: String,
    },
    /// The event stream started (true) or closed (false).
    Following(bool),
}

/// The running view: its state, its channel and what it must do next.
pub(crate) struct ChatSession<S> {
    view: ChatView,
    source: S,
    channel: String,
    /// Public ID of the channel's workspace, known after the first read.
    workspace_id: Option<String>,
    /// When to read the latest messages: at start, after the event stream
    /// starts again and after a failed read.
    latest_due: Option<Instant>,
    /// When to try again a failed read of older messages.
    older_retry_at: Option<Instant>,
    reconnecting: bool,
    read_error: Option<String>,
    dirty: bool,
}

impl<S: ChatSource> ChatSession<S> {
    pub(crate) fn new(channel: String, source: S, offset: time::UtcOffset, now: Instant) -> Self {
        Self {
            view: ChatView::new(channel.clone(), offset),
            source,
            channel,
            workspace_id: None,
            latest_due: Some(now),
            older_retry_at: None,
            reconnecting: false,
            read_error: None,
            dirty: true,
        }
    }

    pub(crate) fn apply_update(&mut self, update: Update, now: Instant) {
        match update {
            Update::Posted {
                workspace_id,
                message,
            } => {
                if self.workspace_id.as_deref() == Some(workspace_id.as_str())
                    && message.channel == self.channel
                {
                    self.view.add_recent([message]);
                    self.dirty = true;
                }
            }
            Update::Renamed {
                workspace_id,
                from,
                channel,
            } => {
                if self.workspace_id.as_deref() == Some(workspace_id.as_str())
                    && from == self.channel
                {
                    self.source.rename(&channel);
                    self.view.channel.clone_from(&channel);
                    self.channel = channel;
                    self.dirty = true;
                }
            }
            Update::Following(true) => {
                // Messages posted while the stream was down come with a new
                // read of the latest ones; the dedup drops the known ones.
                self.reconnecting = false;
                self.latest_due = Some(now);
                self.refresh_notice();
            }
            Update::Following(false) => {
                self.reconnecting = true;
                self.refresh_notice();
            }
        }
    }

    /// Handles a terminal event. Returns true when the user quits.
    pub(crate) fn handle_event(&mut self, event: Event) -> bool {
        match event {
            Event::Key(key) => {
                self.dirty = true;
                match self.view.handle_key(key) {
                    ChatAction::Quit => return true,
                    ChatAction::Send(body) => match self.source.post(body.clone()) {
                        Ok(message) => {
                            self.view.set_notice(None);
                            self.view.add_recent([message]);
                        }
                        Err(err) => {
                            self.view.set_notice(Some(err));
                            self.view.restore_draft(body);
                        }
                    },
                    ChatAction::None => {}
                }
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                let (width, height) = self.view.viewport;
                let area = Rect::new(0, 0, width, height);
                if let Some(thread) = self.view.thread_at(area, mouse.column, mouse.row) {
                    let notice = self.source.open_thread(&thread).err();
                    self.view.set_notice(notice);
                    self.dirty = true;
                }
            }
            Event::Mouse(mouse) => self.dirty |= self.view.handle_mouse(mouse),
            Event::Resize(..) => self.dirty = true,
            _ => {}
        }
        false
    }

    /// Reads from the channel what the view needs now: the latest messages
    /// when they are due, or a page of older ones when the scroll is at the
    /// top.
    pub(crate) fn sync(&mut self, now: Instant) {
        if self.view.wants_latest() {
            self.latest_due = Some(now);
        }
        if self.latest_due.is_some_and(|due| due <= now) {
            match self.source.read(None, PAGE_SIZE) {
                Ok((workspace_id, messages)) => {
                    self.workspace_id = Some(workspace_id);
                    if self.view.wants_latest() || self.view.message_count() == 0 {
                        self.view.replace_with_latest(messages, PAGE_SIZE);
                    } else {
                        self.view.add_recent(messages);
                    }
                    self.latest_due = None;
                    self.read_error = None;
                }
                Err(err) => {
                    self.latest_due = Some(now + RETRY_DELAY);
                    self.read_error = Some(err);
                }
            }
            self.refresh_notice();
            self.dirty = true;
        }
        let older_due = self.older_retry_at.is_none_or(|retry| retry <= now);
        if let (Some(before), true, true) = (
            self.view.wants_older(),
            older_due,
            self.workspace_id.is_some(),
        ) {
            match self.source.read(Some(before), PAGE_SIZE) {
                Ok((_, messages)) => {
                    self.view.add_older(messages, PAGE_SIZE);
                    self.older_retry_at = None;
                }
                Err(err) => {
                    self.older_retry_at = Some(now + RETRY_DELAY);
                    self.view.set_notice(Some(err));
                }
            }
            self.dirty = true;
        }
    }

    fn refresh_notice(&mut self) {
        let notice = match (&self.read_error, self.reconnecting) {
            (Some(err), _) => Some(format!("{err} · retrying")),
            (None, true) => Some("reconnecting".to_owned()),
            (None, false) => None,
        };
        if notice.as_deref() != self.view.notice() {
            self.view.set_notice(notice);
            self.dirty = true;
        }
    }

    /// Draws the view when something changed since the last draw.
    pub(crate) fn draw<B: Backend>(&mut self, terminal: &mut ratatui::Terminal<B>) -> io::Result<()>
    where
        B::Error: Into<io::Error>,
    {
        if !self.dirty {
            return Ok(());
        }
        // The viewport follows the terminal size before the frame is built,
        // so render only reads the view.
        terminal.autoresize().map_err(Into::into)?;
        let area = terminal.get_frame().area();
        self.view.set_viewport(area.width, area.height);
        let view = &self.view;
        terminal
            .draw(|frame| {
                let area = frame.area();
                view.render(area, frame.buffer_mut());
                frame.set_cursor_position(view.cursor(area));
            })
            .map_err(Into::into)?;
        self.dirty = false;
        Ok(())
    }
}

/// The channel through the JSON API socket of the server.
struct ApiSource {
    client: ApiClient,
    workspace_id: Option<String>,
    channel: String,
}

impl ApiSource {
    fn request(&self, id: &str, method: Method) -> Result<ResponseResult, String> {
        let request = Request {
            id: format!("chat:{id}"),
            method,
        };
        self.client
            .request_value_with_timeout(&request, REQUEST_TIMEOUT)
            .and_then(parse_response_value)
            .map(|response| response.result)
            .map_err(|err| err.to_string())
    }
}

impl ChatSource for ApiSource {
    fn read(
        &self,
        before: Option<u64>,
        limit: u32,
    ) -> Result<(String, Vec<ChannelMessageInfo>), String> {
        let result = self.request(
            "read",
            Method::ChannelRead(ChannelReadParams {
                workspace_id: self.workspace_id.clone(),
                channel: self.channel.clone(),
                before,
                limit: Some(limit),
            }),
        )?;
        match result {
            ResponseResult::ChannelMessages {
                workspace_id,
                messages,
                ..
            } => Ok((workspace_id, messages)),
            result => Err(format!("unexpected api result: {result:?}")),
        }
    }

    fn post(&self, body: String) -> Result<ChannelMessageInfo, String> {
        let result = self.request(
            "post",
            Method::ChannelPost(ChannelPostParams {
                workspace_id: self.workspace_id.clone(),
                channel: self.channel.clone(),
                body,
                reply_to: None,
                origin_thread_id: None,
                thread_token: None,
            }),
        )?;
        match result {
            ResponseResult::ChannelMessagePosted { message, .. } => Ok(message),
            result => Err(format!("unexpected api result: {result:?}")),
        }
    }

    fn open_thread(&self, thread_id: &str) -> Result<(), String> {
        self.request(
            "open-thread",
            Method::ThreadOpen(ThreadOpenParams {
                workspace_id: self.workspace_id.clone(),
                thread_id: thread_id.to_owned(),
            }),
        )
        .map(|_| ())
    }

    fn rename(&mut self, channel: &str) {
        self.channel = channel.to_owned();
    }
}

/// Runs the chat view of `channel` until the user quits. `workspace_id` is a
/// public or stable workspace ID; absent means the workspace folder of the
/// current directory, then the active workspace.
pub(crate) fn run(channel: String, workspace_id: Option<String>) -> io::Result<i32> {
    let workspace_id = workspace_id.or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| crate::regents::folder::folder_workspace_id(&cwd))
    });
    let offset = local_offset();
    let client = ApiClient::local();
    let (updates_tx, updates_rx) = mpsc::channel();
    follow_channel(client.clone(), updates_tx);
    let source = ApiSource {
        client,
        workspace_id,
        channel: channel.clone(),
    };
    let mut session = ChatSession::new(channel, source, offset, Instant::now());
    let _screen = Screen::enter()?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(io::stdout()))?;
    loop {
        let now = Instant::now();
        while let Ok(update) = updates_rx.try_recv() {
            session.apply_update(update, now);
        }
        session.sync(now);
        session.draw(&mut terminal)?;
        if event::poll(POLL_INTERVAL)? && session.handle_event(event::read()?) {
            return Ok(0);
        }
    }
}

/// Follows `channel.message_posted` and `channel.renamed` on a background
/// thread and sends each event to `updates`. It subscribes again when the
/// stream closes.
fn follow_channel(client: ApiClient, updates: mpsc::Sender<Update>) {
    let subscribe = Request {
        id: "chat:follow".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![
                Subscription::ChannelMessagePosted {},
                Subscription::ChannelRenamed {},
            ],
        }),
    };
    std::thread::spawn(move || loop {
        if let Ok(stream) = client.subscribe(&subscribe) {
            if updates.send(Update::Following(true)).is_err() {
                return;
            }
            for line in stream.lines() {
                let Ok(line) = line else { break };
                let update = match serde_json::from_str(&line) {
                    Ok(EventEnvelope {
                        data:
                            EventData::ChannelMessagePosted {
                                workspace_id,
                                message,
                            },
                        ..
                    }) => Update::Posted {
                        workspace_id,
                        message,
                    },
                    Ok(EventEnvelope {
                        data:
                            EventData::ChannelRenamed {
                                workspace_id,
                                from,
                                channel,
                            },
                        ..
                    }) => Update::Renamed {
                        workspace_id,
                        from,
                        channel,
                    },
                    _ => continue,
                };
                if updates.send(update).is_err() {
                    return;
                }
            }
        }
        if updates.send(Update::Following(false)).is_err() {
            return;
        }
        std::thread::sleep(RESUBSCRIBE_DELAY);
    });
}

/// Raw mode, the alternate screen and mouse reports while the view runs.
struct Screen;

impl Screen {
    fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let screen = Self;
        crossterm::execute!(
            io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        )?;
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::ChannelAuthorInfo;
    use std::cell::RefCell;

    fn message(id: u64, kind: ChannelAuthorKind, author: &str, body: &str) -> ChannelMessageInfo {
        ChannelMessageInfo {
            id,
            channel: "geral".into(),
            author: ChannelAuthorInfo {
                kind,
                id: author.into(),
            },
            body: body.into(),
            reply_to: None,
            origin_thread_id: None,
            mentions: Vec::new(),
            created_at: "2026-11-27T18:04:05Z".into(),
        }
    }

    fn human(id: u64) -> ChannelMessageInfo {
        message(id, ChannelAuthorKind::Human, "rafael", &id.to_string())
    }

    fn view() -> ChatView {
        ChatView::new("geral", time::UtcOffset::UTC)
    }

    fn screen(view: &mut ChatView, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        view.set_viewport(width, height);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(view: &mut ChatView, text: &str) {
        for ch in text.chars() {
            assert_eq!(view.handle_key(key(KeyCode::Char(ch))), ChatAction::None);
        }
    }

    /// A channel whose answers the test scripts; it records the reads.
    #[derive(Default)]
    struct FakeSource {
        reads: RefCell<Vec<Option<u64>>>,
        answers: RefCell<VecDeque<Result<Vec<ChannelMessageInfo>, String>>>,
        opened: RefCell<Vec<String>>,
    }

    impl FakeSource {
        fn answering(answers: Vec<Result<Vec<ChannelMessageInfo>, String>>) -> Self {
            Self {
                answers: RefCell::new(answers.into()),
                ..Self::default()
            }
        }
    }

    impl ChatSource for &FakeSource {
        fn read(
            &self,
            before: Option<u64>,
            _limit: u32,
        ) -> Result<(String, Vec<ChannelMessageInfo>), String> {
            self.reads.borrow_mut().push(before);
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(Vec::new()))
                .map(|messages| ("w1".to_owned(), messages))
        }

        fn post(&self, _body: String) -> Result<ChannelMessageInfo, String> {
            Err("not in this test".into())
        }

        fn open_thread(&self, thread_id: &str) -> Result<(), String> {
            self.opened.borrow_mut().push(thread_id.to_owned());
            Ok(())
        }
    }

    /// Bytes that the terminal backend writes, shared with the test.
    #[derive(Clone, Default)]
    struct Output(std::rc::Rc<RefCell<Vec<u8>>>);

    impl io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Output {
        /// Bytes written since the last call.
        fn take(&self) -> usize {
            std::mem::take(&mut *self.0.borrow_mut()).len()
        }
    }

    fn terminal() -> (
        ratatui::Terminal<ratatui::backend::CrosstermBackend<Output>>,
        Output,
    ) {
        let output = Output::default();
        let terminal = ratatui::Terminal::with_options(
            ratatui::backend::CrosstermBackend::new(output.clone()),
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 40, 8)),
            },
        )
        .unwrap();
        (terminal, output)
    }

    #[test]
    fn each_message_shows_its_time_and_a_distinct_author_mark() {
        let mut view = view();
        view.add_recent([
            message(1, ChannelAuthorKind::Human, "rafael", "oi"),
            message(2, ChannelAuthorKind::Agent, "chief-of-staff", "olá"),
            message(3, ChannelAuthorKind::System, "regents", "thread bloqueada"),
        ]);

        let lines = screen(&mut view, 40, 9);

        assert_eq!(lines[0], " #geral");
        assert_eq!(lines[1], "── 2026-11-27 ──");
        assert_eq!(lines[2], "18:04 ● rafael: oi");
        assert_eq!(lines[3], "18:04 @chief-of-staff: olá");
        assert_eq!(lines[4], "18:04 system: thread bloqueada");
        assert_eq!(lines[7], "─".repeat(40));
        assert_eq!(lines[8], ">");
    }

    #[test]
    fn times_follow_the_local_offset_and_a_new_day_gets_a_date_line() {
        let mut view = ChatView::new("geral", time::UtcOffset::from_hms(-3, 0, 0).unwrap());
        let mut late = human(1);
        late.created_at = "2026-11-28T02:30:00Z".into();
        let mut next = human(2);
        next.created_at = "2026-11-28T03:10:00Z".into();
        view.add_recent([late, next]);

        let lines = screen(&mut view, 40, 9);

        assert_eq!(
            &lines[1..5],
            [
                "── 2026-11-27 ──",
                "23:30 ● rafael: 1",
                "── 2026-11-28 ──",
                "00:10 ● rafael: 2"
            ]
        );
    }

    #[test]
    fn an_empty_channel_says_so() {
        let mut view = view();

        let lines = screen(&mut view, 40, 6);

        assert_eq!(lines[1], "no messages in #geral yet");
    }

    #[test]
    fn the_history_follows_new_messages_and_scrolls_back() {
        let mut view = view();
        view.add_recent((1..=10).map(human));

        let bottom = screen(&mut view, 30, 6);
        assert_eq!(
            &bottom[1..4],
            [
                "18:04 ● rafael: 8",
                "18:04 ● rafael: 9",
                "18:04 ● rafael: 10"
            ]
        );

        view.handle_key(key(KeyCode::PageUp));
        let older = screen(&mut view, 30, 6);
        assert_eq!(
            &older[1..4],
            [
                "18:04 ● rafael: 6",
                "18:04 ● rafael: 7",
                "18:04 ● rafael: 8"
            ]
        );

        view.handle_key(key(KeyCode::End));
        view.add_recent([human(11)]);
        let followed = screen(&mut view, 30, 6);
        assert_eq!(&followed[3], "18:04 ● rafael: 11");
    }

    #[test]
    fn enter_sends_the_draft_and_clears_it() {
        let mut view = view();
        type_text(&mut view, "olá time");

        assert_eq!(screen(&mut view, 30, 5)[4], "> olá time");
        assert_eq!(
            view.handle_key(key(KeyCode::Enter)),
            ChatAction::Send("olá time".into())
        );
        assert_eq!(screen(&mut view, 30, 5)[4], ">");
        assert_eq!(view.handle_key(key(KeyCode::Enter)), ChatAction::None);
    }

    #[test]
    fn a_blank_draft_is_not_sent() {
        let mut view = view();
        type_text(&mut view, "   ");

        assert_eq!(view.handle_key(key(KeyCode::Enter)), ChatAction::None);
    }

    #[test]
    fn ctrl_c_clears_a_draft_and_then_quits() {
        let mut view = view();
        type_text(&mut view, "rascunho");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(view.handle_key(ctrl_c), ChatAction::None);
        assert_eq!(screen(&mut view, 30, 5)[4], ">");
        assert_eq!(view.handle_key(ctrl_c), ChatAction::Quit);
    }

    #[test]
    fn a_message_seen_twice_shows_once_in_posting_order() {
        let mut view = view();
        view.add_recent([human(2)]);
        view.add_recent([human(1), human(2)]);

        let lines = screen(&mut view, 30, 7);

        assert_eq!(&lines[2..4], ["18:04 ● rafael: 1", "18:04 ● rafael: 2"]);
        assert_eq!(lines[4], "");
    }

    #[test]
    fn the_window_keeps_at_most_the_newest_messages() {
        let mut view = view();
        view.add_recent((1..=MAX_MESSAGES as u64 + 10).map(human));

        assert_eq!(view.message_count(), MAX_MESSAGES);
        view.set_viewport(30, 6);
        view.handle_key(key(KeyCode::Home));
        assert_eq!(view.wants_older(), Some(11));
    }

    #[test]
    fn a_click_on_the_first_line_of_an_agent_message_opens_its_thread() {
        let answer = ChannelMessageInfo {
            origin_thread_id: Some("t3".into()),
            ..message(2, ChannelAuthorKind::Agent, "chief", "olá\nsegunda linha")
        };
        let stray = ChannelMessageInfo {
            // A human message names no thread link, even with an origin.
            origin_thread_id: Some("t9".into()),
            ..human(3)
        };
        let source = FakeSource::answering(vec![Ok(vec![human(1), answer, stray])]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        let (mut terminal, _output) = terminal();
        session.sync(now);
        session.draw(&mut terminal).unwrap();
        let rows = screen(&mut session.view.clone(), 40, 8);
        let click = |row: u16| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let agent_row = rows
            .iter()
            .position(|row| row.contains("@chief [t3]: olá"))
            .unwrap_or_else(|| panic!("{rows:#?}"));

        for row in 0..8 {
            if row != agent_row as u16 {
                session.handle_event(click(row));
            }
        }
        assert!(source.opened.borrow().is_empty(), "{rows:#?}");
        session.handle_event(click(agent_row as u16));

        assert_eq!(*source.opened.borrow(), ["t3"]);
    }

    #[test]
    fn an_idle_view_writes_no_bytes() {
        let source = FakeSource::answering(vec![Ok(vec![human(1)])]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        let (mut terminal, output) = terminal();
        session.sync(now);
        session.draw(&mut terminal).unwrap();
        assert!(output.take() > 0);

        for step in 1..=20 {
            let now = now + Duration::from_millis(100 * step);
            session.sync(now);
            session.draw(&mut terminal).unwrap();
            assert!(!session.handle_event(Event::FocusGained));
        }

        assert_eq!(output.take(), 0);
        assert_eq!(source.reads.borrow().len(), 1);
    }

    #[test]
    fn a_live_message_draws_again() {
        let source = FakeSource::answering(vec![Ok(vec![human(1)])]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        let (mut terminal, output) = terminal();
        session.sync(now);
        session.draw(&mut terminal).unwrap();
        output.take();

        session.apply_update(
            Update::Posted {
                workspace_id: "w1".into(),
                message: human(2),
            },
            now,
        );
        session.draw(&mut terminal).unwrap();

        assert!(output.take() > 0);
    }

    #[test]
    fn a_live_message_of_another_workspace_or_channel_is_ignored() {
        let source = FakeSource::answering(vec![Ok(vec![human(1)])]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        session.sync(now);
        let mut other_channel = human(3);
        other_channel.channel = "design".into();

        session.apply_update(
            Update::Posted {
                workspace_id: "w2".into(),
                message: human(2),
            },
            now,
        );
        session.apply_update(
            Update::Posted {
                workspace_id: "w1".into(),
                message: other_channel,
            },
            now,
        );

        assert_eq!(session.view.message_count(), 1);
    }

    #[test]
    fn a_rename_of_the_channel_moves_the_view_to_the_new_name() {
        let source = FakeSource::answering(vec![Ok(vec![human(1)])]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        session.sync(now);
        let renamed = |workspace_id: &str, from: &str| Update::Renamed {
            workspace_id: workspace_id.into(),
            from: from.into(),
            channel: "anuncios".into(),
        };

        session.apply_update(renamed("w2", "geral"), now);
        session.apply_update(renamed("w1", "design"), now);
        assert_eq!(session.channel, "geral");

        session.apply_update(renamed("w1", "geral"), now);
        let mut moved = human(2);
        moved.channel = "anuncios".into();
        session.apply_update(
            Update::Posted {
                workspace_id: "w1".into(),
                message: moved,
            },
            now,
        );

        assert_eq!(session.channel, "anuncios");
        assert_eq!(session.view.message_count(), 2);
        assert!(screen(&mut session.view, 40, 6)
            .iter()
            .any(|line| line.contains("#anuncios")));
    }

    #[test]
    fn the_stream_starting_reads_the_latest_messages_again_without_duplicates() {
        let source = FakeSource::answering(vec![
            Ok(vec![human(1)]),
            Ok(vec![human(1), human(2), human(3)]),
        ]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        session.sync(now);
        session.apply_update(
            Update::Posted {
                workspace_id: "w1".into(),
                message: human(2),
            },
            now,
        );

        session.apply_update(Update::Following(true), now);
        session.sync(now);

        assert_eq!(session.view.message_count(), 3);
        assert_eq!(source.reads.borrow().as_slice(), [None, None]);
    }

    #[test]
    fn a_failed_read_keeps_the_view_open_and_tries_again() {
        let source = FakeSource::answering(vec![
            Err("channel #geral does not exist in this workspace".into()),
            Ok(vec![human(1)]),
        ]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);

        session.sync(now);
        let failed = screen(&mut session.view, 80, 6);
        assert!(failed[0].contains("does not exist"), "{failed:?}");
        assert!(failed[0].contains("retrying"), "{failed:?}");

        session.sync(now + Duration::from_secs(1));
        assert_eq!(source.reads.borrow().len(), 1);

        session.sync(now + RETRY_DELAY);
        let recovered = screen(&mut session.view, 80, 6);
        assert_eq!(source.reads.borrow().len(), 2);
        assert_eq!(recovered[0], " #geral");
        assert_eq!(recovered[2], "18:04 ● rafael: 1");
    }

    #[test]
    fn reaching_the_top_loads_older_messages() {
        let page: Vec<_> = (300..300 + PAGE_SIZE as u64).map(human).collect();
        let older: Vec<_> = (150..300).map(human).collect();
        let source = FakeSource::answering(vec![Ok(page), Ok(older)]);
        let now = Instant::now();
        let mut session = ChatSession::new("geral".into(), &source, time::UtcOffset::UTC, now);
        session.sync(now);
        session.view.set_viewport(30, 6);

        session.handle_event(Event::Key(key(KeyCode::Home)));
        session.sync(now);

        assert_eq!(source.reads.borrow().as_slice(), [None, Some(300)]);
        assert_eq!(session.view.message_count(), 150 + PAGE_SIZE as usize);
        session.view.set_viewport(30, 6);
        session.handle_event(Event::Key(key(KeyCode::Home)));
        session.sync(now);
        assert_eq!(
            source.reads.borrow().len(),
            2,
            "a short page ends the history"
        );
    }

    #[test]
    fn message_times_parse_only_the_server_format() {
        assert_eq!(
            message_time("2026-11-27T18:04:05Z").map(|time| time.unix_timestamp()),
            Some(1_795_802_645)
        );
        assert_eq!(message_time("2026-11-27 18:04:05"), None);
        assert_eq!(message_time("2026-13-27T18:04:05Z"), None);
    }
}
