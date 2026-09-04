//! The overview one department shows when the operator clicks its rail row.
//!
//! # Why a card and not the department's people
//!
//! A department row used to put every live person in that unit back on the
//! glass at once, tiled. Six agents in a 129x36 window is 42x17 each, so every
//! one of them RENDERED at 42 columns — and the moment the operator clicked one
//! of them it was moved into the full-width focus body and repainted at 129.
//! The operator's report: *"it always starts half screen and then resizes full
//! screen so it's very jarring"*. The grid was the cause: a pane has exactly one
//! size, and a pane that is ever shown in a grid cell is a pane that is
//! rendered at grid-cell width.
//!
//! Their ruling, 2026-08-21: *"every agent lives in its own kind of thing so
//! there's no flickering and glitches, and when I click on the department show
//! me an overview… something simple, something valuable, some good metadata"*.
//!
//! So this card REPLACES the tiled people. It reads nothing from any agent —
//! there are no panes here to resize, nothing scrolls, and no Pi is disturbed
//! by being looked at. It draws the department's own state: who heads it, who
//! is in it, whether each of them is up, and what model each is running.
//!
//! # It is a SNAPSHOT, and the brain owns its freshness
//!
//! The card renders exactly the facts it was launched with and never reads the
//! company itself. That is deliberate: the sibling notice this replaces was
//! measured re-laying its own window on every changefeed wake, which on a
//! chatty company is many times a second, and the glass churned continuously.
//! The brain repaints this card only when the department's facts actually
//! change (`brain::department_card_launch`), which is the same transition-only
//! discipline `effects::show_sleeping_department` documents at length.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};

use super::client::Glass;
use super::PersonState;

/// How a state is drawn: the glyph the strip and the table share, the word the
/// table prints, and which bucket the roll-up counts it in.
///
/// The STATES themselves are [`PersonState`] — the product's one definition of
/// what a person is doing, which the rail beside this card already uses. A
/// second vocabulary here would be a second answer to the same question, and
/// the two would drift the first time a state was added.
const fn glyph(state: PersonState) -> &'static str {
    match state {
        PersonState::Working => "\u{25cf}",
        // A SYMMETRIC glyph, and that is the whole reason it is this one.
        // `\u{25d0}` (a circle with its left half filled) shares no optical
        // centre with `\u{25cb}` in most terminal fonts — the filled mass reads
        // as sitting LOW against the hollow ring beside it, which is what an
        // operator saw and reported as "the half circle is always like down".
        // A concentric glyph cannot have that fault: its mass is distributed
        // evenly about the centre by construction, so it sits on the same
        // optical line as `\u{25cf}` and `\u{25cb}` whatever the font does.
        // Not `\u{25c9}`, which `render.rs` already spends on `Crashing`: one
        // state, one glyph, across both surfaces.
        PersonState::Idle => "\u{25ce}",
        PersonState::Starting => "\u{25cc}",
        PersonState::Refused | PersonState::Crashing => "\u{2715}",
        PersonState::Sleeping => "\u{25cb}",
    }
}

/// The word the table prints for a state.
///
/// `Refused` reads "cannot start" rather than "refused": the operator's own
/// vocabulary for it, and the same words `sleeping_card` puts where its button
/// would be, so one company says one thing about one person.
const fn label(state: PersonState) -> &'static str {
    match state {
        PersonState::Working => "working",
        PersonState::Idle => "idle",
        PersonState::Starting => "starting",
        PersonState::Refused => "cannot start",
        PersonState::Crashing => "crashing",
        PersonState::Sleeping => "asleep",
    }
}

/// Which roll-up bucket a state counts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// Has a pane: working or idle.
    Up,
    /// Wanted, no pane yet.
    Starting,
    /// Wanted and cannot run — the launch gate declined, or the boot keeps
    /// dying. NEVER folded into `Asleep`: settled is the product working and
    /// blocked is a fault an operator can act on, and a card that merged them
    /// would make a broken company look like a quiet one.
    Blocked,
    /// Settled. Clicking them wakes them.
    Asleep,
}

/// The bucket for one state.
#[must_use]
pub const fn bucket(state: PersonState) -> Bucket {
    match state {
        PersonState::Working | PersonState::Idle => Bucket::Up,
        PersonState::Starting => Bucket::Starting,
        PersonState::Refused | PersonState::Crashing => Bucket::Blocked,
        PersonState::Sleeping => Bucket::Asleep,
    }
}

/// One row of the department table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Member {
    /// Roster display name.
    pub name: String,
    /// Exact company role from the roster.
    pub role: String,
    /// What this person is doing, in the product's one vocabulary.
    pub state: PersonState,
    /// The effective model, already rendered by the brain — provider and model
    /// joined, or empty when the backend has no answer. The card does no
    /// provider resolution of its own; that is `launch_catalog`'s job and one
    /// implementation of it is enough.
    pub model: String,
    /// Messages still in this person's durable inbox view.
    pub inbox_messages: usize,
    /// Whether this person heads the department.
    pub head: bool,
}

/// Everything the card draws.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Card {
    /// The department's display name.
    pub name: String,
    /// The ancestor chain, outermost first, EXCLUDING this department. Empty
    /// for the root. Drawn so an operator can tell two same-named units apart
    /// — the tree is recursive and "Research" under two parents is two
    /// departments.
    pub path: Vec<String>,
    /// Members in roster order.
    pub members: Vec<Member>,
    /// Sub-department names, in the company's canonical order.
    pub children: Vec<String>,
}

impl Card {
    /// How many members fall in each roll-up bucket: up, starting, blocked,
    /// asleep.
    #[must_use]
    pub fn tally(&self) -> Tally {
        let mut tally = Tally::default();
        for member in &self.members {
            match bucket(member.state) {
                Bucket::Up => tally.up += 1,
                Bucket::Starting => tally.starting += 1,
                Bucket::Blocked => tally.blocked += 1,
                Bucket::Asleep => tally.asleep += 1,
            }
        }
        tally
    }

    /// The department head's display name, if this unit has one.
    #[must_use]
    pub fn head(&self) -> Option<&Member> {
        self.members.iter().find(|member| member.head)
    }

    /// The models in use, most-used first, with a count each.
    ///
    /// An operator asked for this by asking "what their model is" — but the
    /// per-person answer is already in the table, and a department of nineteen
    /// people is nineteen lines to compare by eye. The roll-up answers the
    /// question the table cannot: *is this unit on one model or four?*
    #[must_use]
    pub fn models(&self) -> Vec<(String, usize)> {
        let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for member in &self.members {
            let label = member.model.trim();
            if label.is_empty() {
                continue;
            }
            *counts.entry(label).or_default() += 1;
        }
        let mut ordered: Vec<(String, usize)> =
            counts.into_iter().map(|(label, count)| (label.to_owned(), count)).collect();
        // Most-used first; ties keep the name order the BTreeMap fixed, so the
        // strip is stable between repaints and does not shuffle under the eye.
        ordered.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        ordered
    }
}

/// The roll-up counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    /// Has a pane.
    pub up: usize,
    /// Wanted, no pane yet.
    pub starting: usize,
    /// Wanted and cannot run.
    pub blocked: usize,
    /// Settled.
    pub asleep: usize,
}

/// The `\u{25cf} \u{25cf} \u{25cf} \u{25cb} \u{25cb}` strip: one glyph per member, the working half first.
///
/// # This is a CELL budget, not a glyph count
///
/// The rule it enforces is the one its test is named for: a large department
/// must not push the counts off the line. That rule is about WIDTH, and it used
/// to be spelled as a glyph count only because the glyphs were packed with no
/// separator. They are separated now — the run was unreadable jammed together,
/// which the operator reported — so a spaced run of `n` glyphs occupies
/// `2n - 1` cells and the glyph count is derived from this budget rather than
/// being it. Change this number and the strip gets wider or narrower; the
/// counts beside it stay on the line either way.
///
/// Beyond the budget the strip stops being a picture of the unit and starts
/// being a wall, and the numbers beside it are the answer anyway.
const STRIP_CAP: usize = 24;

/// How many glyphs fit in [`STRIP_CAP`] cells once each is separated by one
/// space: `n + (n - 1) <= STRIP_CAP`.
const fn strip_glyph_cap() -> usize {
    STRIP_CAP.saturating_add(1) / 2
}

#[must_use]
fn strip(card: &Card) -> String {
    // The working half first: the strip reads as "how much of this unit is
    // working", so that half belongs where the eye lands.
    let mut ordered: Vec<(u8, &str)> = card
        .members
        .iter()
        .map(|member| {
            let rank = match bucket(member.state) {
                Bucket::Up => 0,
                Bucket::Starting => 1,
                Bucket::Blocked => 2,
                Bucket::Asleep => 3,
            };
            (rank, glyph(member.state))
        })
        .collect();
    ordered.sort_by_key(|(rank, _)| *rank);
    let mut glyphs: Vec<&str> = ordered.into_iter().map(|(_, glyph)| glyph).collect();
    glyphs.truncate(strip_glyph_cap());
    // ONE SPACE BETWEEN GLYPHS. Packed, the run read as a single smeared bar
    // rather than as a count of people — the operator asked for the space, and
    // the separation is what makes the boundary between two states legible at
    // a glance.
    glyphs.join(" ")
}

/// The horizontal bar under the strip, `width` cells wide.
///
/// Rounds DOWN and never claims a full bar for a partial unit: a department
/// with one sleeper must not draw the same bar as one with none. The one
/// exception is the other end — a single awake person in a large unit still
/// gets one filled cell, because zero filled cells is the picture of nobody up.
#[must_use]
fn bar(awake: usize, total: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if total == 0 {
        return "░".repeat(width);
    }
    let exact = awake * width / total;
    let filled = if awake > 0 { exact.max(1) } else { 0 };
    let filled = filled.min(width);
    format!("{}{}", "▓".repeat(filled), "░".repeat(width - filled))
}

/// Cut `text` to `width` characters, ending in an ellipsis when it had to cut.
///
/// Counts CHARACTERS, the same rule and for the same reason as the rail's own
/// `render::fit`: every glyph this surface draws is one terminal cell.
#[must_use]
fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    if width == 1 {
        return "\u{2026}".to_owned();
    }
    let kept: String = text.chars().take(width - 1).collect();
    format!("{kept}\u{2026}")
}

/// Palette. Two, because a pane inherits the operator's terminal and a card
/// that assumes dark is unreadable on half of them — the same reason
/// `sleeping_card` carries both.
struct Palette {
    head: Color,
    dim: Color,
    awake: Color,
    asleep: Color,
    blocked: Color,
}

const fn palette(light: bool) -> Palette {
    if light {
        Palette {
            head: Color::Rgb(30, 30, 30),
            dim: Color::Rgb(120, 120, 120),
            awake: Color::Rgb(20, 120, 60),
            asleep: Color::Rgb(140, 140, 140),
            blocked: Color::Rgb(170, 40, 40),
        }
    } else {
        Palette {
            head: Color::Rgb(235, 235, 235),
            dim: Color::Rgb(140, 140, 140),
            awake: Color::Rgb(90, 200, 130),
            asleep: Color::Rgb(130, 130, 130),
            blocked: Color::Rgb(230, 110, 110),
        }
    }
}

/// The header block: the department's name and where it sits in the tree.
fn draw_header(frame: &mut Frame<'_>, area: Rect, card: &Card, palette: &Palette) {
    let width = usize::from(area.width).saturating_sub(2);
    let mut lines = vec![Line::from(vec![Span::styled(
        fit(&card.name, width),
        Style::default().fg(palette.head).add_modifier(Modifier::BOLD),
    )])];
    if !card.path.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            fit(&card.path.join(" / "), width),
            Style::default().fg(palette.dim),
        )]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The roll-up: the strip, the bar, the counts and the models in use.
fn draw_rollup(frame: &mut Frame<'_>, area: Rect, card: &Card, palette: &Palette) {
    let width = usize::from(area.width);
    let tally = card.tally();
    let (awake, asleep, blocked, starting) =
        (tally.up, tally.asleep, tally.blocked, tally.starting);
    let total = card.members.len();
    let mut lines = Vec::new();

    let strip_text = strip(card);
    // Only the buckets that have somebody in them. A line that always prints
    // "0 cannot start" trains the eye to skip the place a real fault appears.
    let mut counts = vec![format!("{awake} up"), format!("{asleep} asleep")];
    if starting > 0 {
        counts.push(format!("{starting} starting"));
    }
    if blocked > 0 {
        counts.push(format!("{blocked} cannot start"));
    }
    let counts = counts.join(" · ");
    lines.push(Line::from(vec![
        Span::styled(format!("{strip_text}  "), Style::default().fg(palette.awake)),
        Span::styled(counts, Style::default().fg(palette.dim)),
    ]));

    // The bar takes the width the counts do not, floored so a narrow pane draws
    // no bar rather than a misleading stub.
    let percent = (awake * 100).checked_div(total).unwrap_or_default();
    let tail = format!(" {percent}% of the unit is up");
    let bar_width = width.saturating_sub(tail.chars().count() + 2).min(48);
    if bar_width >= 8 {
        lines.push(Line::from(vec![
            Span::styled(bar(awake, total, bar_width), Style::default().fg(palette.awake)),
            Span::styled(tail, Style::default().fg(palette.dim)),
        ]));
    }

    let models = card.models();
    if !models.is_empty() {
        let rendered = models
            .iter()
            .map(
                |(label, count)| {
                    if *count == 1 {
                        label.clone()
                    } else {
                        format!("{label} ×{count}")
                    }
                },
            )
            .collect::<Vec<_>>()
            .join("  ·  ");
        lines.push(Line::from(vec![Span::styled(
            fit(&format!("models  {rendered}"), width),
            Style::default().fg(palette.dim),
        )]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// One cell between each of the five columns.
const COLUMN_SPACING: u16 = 4;

/// Where each column sits in the `[name, role, state, inbox, model]` array.
const NAME: usize = 0;
const ROLE: usize = 1;
const STATE: usize = 2;
const INBOX: usize = 3;
const MODEL: usize = 4;

/// The glyph and its trailing space, in front of every name.
const GLYPH_WIDTH: u16 = 2;

/// ` (head)`, plus the space [`fit`] is given back so the marker is never the
/// thing that gets cut.
const HEAD_MARKER_WIDTH: u16 = 8;

/// What a column is cut down to before the next one gives up anything, and the
/// order they give: role first, then name, then state, and the model LAST.
///
/// The model is last because it is the column an operator asked for by name,
/// and the role is first because it is the one they can most afford to lose —
/// a truncated job title is still recognisable, a truncated model id is not.
/// The narrowest the model column may be made while anything else still has
/// cells to give. Below this a model id is unrecognisable, which is the same as
/// not drawing it.
const MODEL_FLOOR: u16 = 12;

/// What each column needs to draw every one of `members` in full.
fn wants(members: &[Member]) -> [u16; 5] {
    let longest = |pick: &dyn Fn(&Member) -> usize| -> u16 {
        u16::try_from(members.iter().map(pick).max().unwrap_or_default()).unwrap_or(u16::MAX)
    };
    [
        longest(&|member| member.name.chars().count()).saturating_add(GLYPH_WIDTH),
        longest(&|member| {
            member.role.chars().count()
                + if member.head { usize::from(HEAD_MARKER_WIDTH) } else { 0 }
        }),
        longest(&|member| label(member.state).chars().count()),
        longest(&|member| member.inbox_messages.to_string().chars().count()).max(5),
        longest(&|member| member.model.chars().count()),
    ]
}

/// Column widths for the member table — `[name, role, state, inbox, model]` — given
/// the pane's width and the people it has to draw.
///
/// # It allocates from the CONTENT, and filling the pane is not the goal
///
/// The first version allocated from the WIDTH alone, in fractions with fixed
/// caps: `model` at 30 and `role` at 36. On the operator's ~200-column pane
/// that drew a 95-column table with a hundred columns of nothing beside it,
/// and cut `openrouter/deepseek/deepseek-chat-v3.1` to
/// `openrouter/deepseek/deepseek-…` — a column clipped for want of room in a
/// pane that was half empty.
///
/// The cap it replaces was not arbitrary, and the fix must not simply lift it:
/// an UNCAPPED role column measured 214 cells at 273 columns, which pushed the
/// model against the right edge with a corridor of blank in between and made
/// the table unreadable as a table. So the rule is neither cap nor stretch:
/// **every column gets exactly what its longest cell needs, and gives cells up
/// only when the pane genuinely cannot hold them.** A wide pane draws a table
/// as wide as its content and no wider; the blank to the right of it is the
/// correct picture of a short answer, not wasted space.
///
/// Public so the tests can pin the two invariants that outlive any of this
/// arithmetic: the five columns plus their spacing never overflow the pane at
/// ANY width, and the model column is the last one to give up a cell.
#[must_use]
pub fn columns(width: u16, members: &[Member]) -> [u16; 5] {
    let usable = width.saturating_sub(COLUMN_SPACING);
    let mut given = wants(members);
    let mut over = given.iter().copied().sum::<u16>().saturating_sub(usable);
    // The inbox header may give way, but the decimal answer never does. This
    // keeps a large count exact before the model gives up a cell.
    let inbox_floor = members
        .iter()
        .map(|member| member.inbox_messages.to_string().chars().count())
        .max()
        .and_then(|width| u16::try_from(width).ok())
        .unwrap_or_default();
    for (column, floor) in
        [(ROLE, 0), (NAME, 6), (STATE, 4), (INBOX, inbox_floor), (MODEL, MODEL_FLOOR)]
    {
        if over == 0 {
            break;
        }
        let take = given[column].saturating_sub(floor).min(over);
        given[column] -= take;
        over -= take;
    }
    // EVEN THE FLOORS MAY NOT FIT. tmux hands out one-cell panes during a
    // relayout, and a table that answered such a pane with negative arithmetic
    // would draw past it. This hands out what there is in priority order and
    // leaves the columns past the end at zero; when the floors DO fit it
    // changes nothing.
    let mut left = usable;
    let mut fitted = [0_u16; 5];
    for column in [NAME, INBOX, MODEL, STATE, ROLE] {
        fitted[column] = if column == INBOX && given[column] > left {
            // A partial decimal is a different number. Hide this column when
            // the pane cannot hold every digit; the next wider frame restores
            // it whole.
            0
        } else {
            given[column].min(left)
        };
        left -= fitted[column];
    }
    fitted
}

/// The member table.
fn draw_members(frame: &mut Frame<'_>, area: Rect, card: &Card, palette: &Palette) {
    let [name_w, role_w, state_w, inbox_w, model_w] = columns(area.width, &card.members);
    let rows: Vec<Row<'_>> = card
        .members
        .iter()
        .map(|member| {
            let state = member.state;
            let colour = match bucket(state) {
                Bucket::Up => palette.awake,
                Bucket::Starting => palette.dim,
                Bucket::Blocked => palette.blocked,
                Bucket::Asleep => palette.asleep,
            };
            let name = format!(
                "{} {}",
                glyph(state),
                fit(&member.name, usize::from(name_w.saturating_sub(GLYPH_WIDTH)))
            );
            let role = if member.head {
                format!(
                    "{} (head)",
                    fit(&member.role, usize::from(role_w.saturating_sub(HEAD_MARKER_WIDTH)))
                )
            } else {
                fit(&member.role, usize::from(role_w))
            };
            Row::new(vec![
                Span::styled(name, Style::default().fg(colour)),
                Span::styled(role, Style::default().fg(palette.dim)),
                Span::styled(fit(label(state), usize::from(state_w)), Style::default().fg(colour)),
                Span::styled(
                    format!("{:>width$}", member.inbox_messages, width = usize::from(inbox_w)),
                    Style::default().fg(palette.dim),
                ),
                Span::styled(
                    fit(&member.model, usize::from(model_w)),
                    Style::default().fg(palette.dim),
                ),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(name_w),
            Constraint::Length(role_w),
            Constraint::Length(state_w),
            Constraint::Length(inbox_w),
            Constraint::Length(model_w),
        ],
    )
    .header(Row::new(vec![
        Span::raw(""),
        Span::raw(""),
        Span::raw(""),
        Span::styled(
            format!("{:>width$}", "inbox", width = usize::from(inbox_w)),
            Style::default().fg(palette.dim),
        ),
        Span::raw(""),
    ]))
    .column_spacing(1);
    frame.render_widget(table, area);
}

/// The sub-department footer.
fn draw_children(frame: &mut Frame<'_>, area: Rect, card: &Card, palette: &Palette) {
    if card.children.is_empty() {
        return;
    }
    let width = usize::from(area.width);
    let text = format!("units  {}", card.children.join("  ·  "));
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            fit(&text, width),
            Style::default().fg(palette.dim),
        )])),
        area,
    );
}

/// Draw the whole card.
pub fn draw(frame: &mut Frame<'_>, card: &Card, light: bool) {
    let palette = palette(light);
    let block = Block::default().borders(Borders::ALL).padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());

    let header_height = if card.path.is_empty() { 1 } else { 2 };
    let rollup_height = 3_u16.min(inner.height.saturating_sub(header_height + 1));
    let children_height = u16::from(!card.children.is_empty());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Length(1),
            Constraint::Length(rollup_height),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(children_height),
        ])
        .split(inner);

    draw_header(frame, chunks[0], card, &palette);
    draw_rollup(frame, chunks[2], card, &palette);
    draw_members(frame, chunks[4], card, &palette);
    draw_children(frame, chunks[5], card, &palette);
}

/// Whether the operator's terminal is light. Same environment read the sleeping
/// card uses, so two cards on one glass never disagree about the palette.
fn is_light() -> bool {
    std::env::var("COLORFGBG").is_ok_and(|value| {
        value
            .rsplit(';')
            .next()
            .and_then(|back| back.parse::<u8>().ok())
            .is_some_and(|back| back > 6)
    })
}

/// Run the card until the pane is killed.
///
/// It answers no gesture and asks the brain for nothing: the whole surface is a
/// report. `q` and `Esc` are accepted so a operator who lands here from a
/// keyboard is not trapped, and everything else is ignored rather than
/// forwarded — a card that swallowed clicks would take them away from the rail
/// beside it.
///
/// # Errors
/// Any terminal failure.
pub fn run(_company_dir: &Path, card: Card) -> std::io::Result<()> {
    let _glass = Glass::take()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let light = is_light();
    loop {
        terminal.draw(|frame| draw(frame, &card, light))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                {
                    break;
                }
            }
        }
        std::io::stdout().flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
