//! Unit tests for the department overview card.
//!
//! The card is a REPORT, so every test here is about one claim: does the
//! picture it draws agree with the facts it was given. A card that rounds a
//! partial unit up to a full bar, or that silently drops the model column on a
//! narrow pane, is a surface that lies quietly — which is worse than one that
//! fails, because nobody goes looking.

use super::*;
use ratatui::backend::TestBackend;

fn member(name: &str, role: &str, state: PersonState, model: &str, head: bool) -> Member {
    Member {
        name: name.to_owned(),
        role: role.to_owned(),
        state,
        model: model.to_owned(),
        inbox_messages: 0,
        head,
    }
}

fn engineering() -> Card {
    let mut members = vec![
        member("Ada", "Head of Engineering", PersonState::Working, "deepseek-v4-flash", true),
        member("Owen", "Planner", PersonState::Working, "deepseek-v4-flash", false),
        member("Rhea", "Software Engineer", PersonState::Sleeping, "deepseek-v4-flash", false),
        member("Kai", "Software Engineer", PersonState::Working, "glm-5.2", false),
        member("Wren", "Code Reviewer", PersonState::Refused, "", false),
    ];
    members[1].inbox_messages = 12;
    Card {
        name: "Engineering".to_owned(),
        path: vec!["Taperoom Inc".to_owned()],
        members,
        children: vec!["Platform".to_owned()],
    }
}

#[test]
fn the_tally_counts_every_bucket_separately() {
    let tally = engineering().tally();
    assert_eq!((tally.up, tally.asleep, tally.blocked, tally.starting), (3, 1, 1, 0));
}

/// "Asleep" and "cannot start" are different sentences to an operator: the
/// first is the product working, the second is a fault. A card that merged them
/// would make a broken company look like a quiet one.
#[test]
fn a_blocked_member_is_never_counted_as_asleep() {
    let card = engineering();
    let tally = card.tally();
    assert_eq!(tally.asleep, 1, "only Rhea is asleep");
    assert_eq!(tally.blocked, 1, "Wren cannot start and says so");
    let wren = card.members.iter().find(|m| m.name == "Wren").expect("wren");
    assert_eq!(bucket(wren.state), Bucket::Blocked);
}

/// EVERY state the product can establish lands in a bucket, and the two that
/// mean "not running for different reasons" never share one. A card that folded
/// `Refused` into `Sleeping` would make a broken company look like a quiet one;
/// one that folded `Crashing` into `Starting` would report a boot loop as
/// progress, which is the exact defect `PersonState::Crashing` exists to end.
#[test]
fn every_person_state_lands_in_a_bucket_and_the_faults_stay_separate() {
    assert_eq!(bucket(PersonState::Working), Bucket::Up);
    assert_eq!(bucket(PersonState::Idle), Bucket::Up);
    assert_eq!(bucket(PersonState::Starting), Bucket::Starting);
    assert_eq!(bucket(PersonState::Refused), Bucket::Blocked);
    assert_eq!(bucket(PersonState::Crashing), Bucket::Blocked);
    assert_eq!(bucket(PersonState::Sleeping), Bucket::Asleep);
}

/// The word on the row is the operator's vocabulary, not the enum's. `refused`
/// is the product's internal name; "cannot start" is what the sleeping card
/// already says where its button would be, so one company says one thing.
#[test]
fn a_refused_person_reads_cannot_start_and_not_refused() {
    assert_eq!(label(PersonState::Refused), "cannot start");
    assert_eq!(label(PersonState::Sleeping), "asleep");
    assert_eq!(label(PersonState::Idle), "idle");
}

#[test]
fn the_head_is_found_by_the_flag_and_not_by_position() {
    let mut card = engineering();
    card.members.rotate_left(3);
    assert_eq!(card.head().expect("a head").name, "Ada");
}

#[test]
fn a_department_with_no_head_says_so_rather_than_electing_one() {
    let card = Card {
        name: "Orphans".to_owned(),
        path: Vec::new(),
        members: vec![member("Bo", "Analyst", PersonState::Working, "glm-5.2", false)],
        children: Vec::new(),
    };
    assert!(card.head().is_none());
}

/// The roll-up answers the question the per-person table cannot: is this unit
/// on one model or four?
#[test]
fn the_model_rollup_is_most_used_first_and_ignores_the_unknown() {
    let models = engineering().models();
    assert_eq!(
        models,
        vec![("deepseek-v4-flash".to_owned(), 3), ("glm-5.2".to_owned(), 1)],
        "Wren has no model fact and must not appear as an empty one"
    );
}

/// Ties keep a fixed order so the strip does not shuffle between repaints.
#[test]
fn equal_model_counts_are_ordered_by_name_so_repaints_are_stable() {
    let card = Card {
        name: "Desk".to_owned(),
        path: Vec::new(),
        members: vec![
            member("A", "r", PersonState::Working, "zeta", false),
            member("B", "r", PersonState::Working, "alpha", false),
        ],
        children: Vec::new(),
    };
    assert_eq!(card.models(), vec![("alpha".to_owned(), 1), ("zeta".to_owned(), 1)]);
}

/// The strip reads as "how much of this unit is working", so the working half
/// belongs where the eye lands.
#[test]
fn the_strip_puts_the_working_half_first() {
    assert_eq!(
        strip(&engineering()),
        "\u{25cf} \u{25cf} \u{25cf} \u{2715} \u{25cb}",
        "the order is unchanged; the glyphs are separated so the run reads as people"
    );
}

/// THE OPERATOR'S TWO FAULTS, pinned so neither comes back.
///
/// Packed, the run read as one smeared bar instead of a count of people. And
/// `\u{25d0}` — a circle with its left half filled — shares no optical centre
/// with the hollow ring beside it in most terminal fonts, so it read as sitting
/// low: *"the half circle is always like down."*
#[test]
fn the_strip_is_spaced_and_every_glyph_is_symmetric_about_its_centre() {
    let card = Card {
        name: "Mixed".to_owned(),
        path: Vec::new(),
        members: vec![
            member("a", "r", PersonState::Working, "m", false),
            member("b", "r", PersonState::Idle, "m", false),
            member("c", "r", PersonState::Starting, "m", false),
            member("d", "r", PersonState::Sleeping, "m", false),
        ],
        children: Vec::new(),
    };
    let drawn = strip(&card);
    assert!(drawn.contains(' '), "the glyphs are separated: {drawn:?}");
    assert!(
        !drawn.contains("\u{25d0}"),
        "the half-filled circle is the fault being fixed, not a state: {drawn:?}"
    );

    // Every glyph the strip can draw is ONE character. `render.rs::fit` counts
    // characters to truncate, on the stated ground that every glyph the rail
    // draws is one cell — a two-cell glyph here would silently break truncation
    // on every row that used it.
    for state in [
        PersonState::Working,
        PersonState::Idle,
        PersonState::Starting,
        PersonState::Refused,
        PersonState::Crashing,
        PersonState::Sleeping,
    ] {
        assert_eq!(
            glyph(state).chars().count(),
            1,
            "{state:?} must draw in exactly one cell: {:?}",
            glyph(state)
        );
    }

    // And the states stay TELLABLE APART, which is the thing a prettier glyph
    // could quietly cost. `Refused` and `Crashing` deliberately share one word
    // on this surface; every other state is its own.
    let distinct = [
        glyph(PersonState::Working),
        glyph(PersonState::Idle),
        glyph(PersonState::Starting),
        glyph(PersonState::Sleeping),
        glyph(PersonState::Refused),
    ];
    let mut seen: Vec<&str> = distinct.to_vec();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), distinct.len(), "two states drawn the same is no picture at all");
}

#[test]
fn a_very_large_department_does_not_push_the_counts_off_the_line() {
    let card = Card {
        name: "Big".to_owned(),
        path: Vec::new(),
        members: (0..60)
            .map(|index| member(&format!("p{index}"), "r", PersonState::Working, "m", false))
            .collect(),
        children: Vec::new(),
    };
    // The rule this test is NAMED for is about WIDTH, and it still holds: the
    // spaced run fits the same cell budget the packed one did, so nothing moved
    // the counts. It shows fewer PEOPLE to do it, which is the trade the
    // spacing costs and is recorded rather than hidden — the numbers beside the
    // strip are the census, the strip is the texture.
    let drawn = strip(&card);
    assert!(
        drawn.chars().count() <= STRIP_CAP,
        "the strip must not push the counts off the line: {} cells",
        drawn.chars().count()
    );
    assert_eq!(
        drawn.chars().filter(|c| *c != ' ').count(),
        strip_glyph_cap(),
        "a large department fills the budget exactly rather than stopping short"
    );
}

/// THE ROUNDING RULE, both ends. A unit with one sleeper must not draw the same
/// bar as one with none, and a unit with one person up must not draw the same
/// bar as one with nobody up.
#[test]
fn the_bar_never_rounds_a_partial_unit_up_to_full() {
    let full = bar(10, 10, 10);
    let almost = bar(9, 10, 10);
    assert_eq!(full, "▓▓▓▓▓▓▓▓▓▓");
    assert_ne!(almost, full, "nine of ten is not ten of ten");
    assert_eq!(almost, "▓▓▓▓▓▓▓▓▓░");
}

#[test]
fn one_person_up_in_a_large_unit_still_draws_one_cell() {
    assert_eq!(bar(1, 40, 10), "▓░░░░░░░░░", "a floor of zero would draw the picture of nobody up");
}

#[test]
fn nobody_up_draws_an_empty_bar_and_never_a_filled_cell() {
    assert_eq!(bar(0, 6, 6), "░░░░░░");
}

#[test]
fn an_empty_department_draws_an_empty_bar_rather_than_dividing_by_zero() {
    assert_eq!(bar(0, 0, 4), "░░░░");
}

#[test]
fn a_zero_width_bar_is_empty_and_does_not_panic() {
    assert_eq!(bar(3, 4, 0), "");
}

#[test]
fn fit_cuts_with_an_ellipsis_and_keeps_what_fits() {
    assert_eq!(fit("Head of Engineering", 40), "Head of Engineering");
    assert_eq!(fit("Head of Engineering", 8), "Head of\u{2026}");
    assert_eq!(fit("abc", 1), "\u{2026}");
    assert_eq!(fit("abc", 0), "");
}

/// The DEFECT this file's column rules were rewritten for, stated as a test.
///
/// The operator's pane was about 200 columns. The table drew 95 of them and cut
/// `openrouter/deepseek/deepseek-chat-v3.1` to `openrouter/deepseek/deepseek-…`
/// with a hundred columns of nothing to the right of it — a column clipped for
/// want of room in a pane that was half empty, because `model` was capped at a
/// flat 30 cells whatever the pane could hold.
#[test]
fn a_wide_pane_draws_a_long_model_whole_rather_than_clipping_it_beside_blank_space() {
    let model = "openrouter/deepseek/deepseek-chat-v3.1";
    let card = Card {
        name: "Executive".to_owned(),
        path: Vec::new(),
        members: vec![
            member("Chief", "Chief Executive Officer", PersonState::Working, model, true),
            member("Sam", "Chief of Staff", PersonState::Working, model, false),
        ],
        children: Vec::new(),
    };
    let [_, _, _, _, model_w] = columns(200, &card.members);
    let width = u16::try_from(model.chars().count()).expect("a model id fits a u16");
    assert_eq!(model_w, width, "a 200-column pane can hold this model whole");
    assert_eq!(fit(model, usize::from(model_w)), model, "so it is drawn whole, with no ellipsis");
}

/// The same rule for every other column: what a pane can hold, it draws. A
/// `cannot start` cut to `cannot s` is the state column telling the operator
/// less than it knows for no reason at all.
#[test]
fn a_wide_pane_draws_every_column_whole_including_the_head_marker() {
    let card = engineering();
    let [name_w, role_w, state_w, inbox_w, model_w] = columns(200, &card.members);
    assert_eq!(name_w, 6, "the longest name is four characters, plus the glyph and its space");
    assert_eq!(role_w, 27, "`Head of Engineering` plus room for ` (head)`");
    assert_eq!(state_w, 12, "`cannot start`, whole");
    assert_eq!(inbox_w, 5, "the `inbox` header is whole");
    assert_eq!(model_w, 17, "`deepseek-v4-flash`, whole");
    let ada = &card.members[0];
    assert_eq!(
        fit(&ada.role, usize::from(role_w - 8)),
        ada.role,
        "the head's role survives the marker it has to make room for"
    );
}

/// A table as wide as its content and no wider. Stretching the columns to fill
/// the pane is NOT the repair, and this test exists so nobody makes it one: an
/// uncapped role column measured 214 cells at 273 columns and left a corridor
/// of blank between the role and the model, which is what the cap that clipped
/// the model was added to stop.
#[test]
fn a_wide_pane_does_not_stretch_the_columns_to_fill_it() {
    let card = engineering();
    let narrow = columns(120, &card.members);
    let wide = columns(400, &card.members);
    assert_eq!(narrow, wide, "past the point the content fits, more pane changes nothing");
}

/// The model column is the one an operator asked for by name. The role column
/// is the one they can most afford to lose, so it absorbs a narrow pane first.
#[test]
fn a_narrow_pane_keeps_the_model_column_and_shrinks_the_role() {
    let card = engineering();
    let [_, wide_role, _, _, wide_model] = columns(129, &card.members);
    let [_, narrow_role, _, _, narrow_model] = columns(60, &card.members);
    assert!(narrow_role < wide_role, "the role column gives way first");
    assert!(narrow_model >= MODEL_FLOOR, "the model column never collapses: {narrow_model}");
    assert_eq!(narrow_model, wide_model, "and it gives up nothing while the role still can");
}

/// THE SHRINK ORDER, in full. The model is the LAST column to lose a cell, and
/// it only loses one once every other column is down to its own floor.
#[test]
fn the_model_is_the_last_column_to_give_up_a_cell() {
    let card = engineering();
    let mut previous = columns(u16::MAX, &card.members)[4];
    let mut model_shrank_at = None;
    for width in (10..=140_u16).rev() {
        let [name, role, state, inbox, model] = columns(width, &card.members);
        if model < previous {
            model_shrank_at = Some(width);
            assert_eq!(role, 0, "the role had nothing left to give at width {width}");
            assert_eq!(name, 6, "and the name was at its floor: {name}");
            assert_eq!(state, 4, "and so was the state: {state}");
            assert_eq!(inbox, 2, "and the two-digit inbox answer stayed whole: {inbox}");
            break;
        }
        previous = model;
    }
    assert!(model_shrank_at.is_some(), "the model does give way once nothing else can");
}

/// THE ARITHMETIC MUST NOT OVERFLOW THE PANE, at any width, ever. The first
/// version used fixed widths and `state + name` alone was 26 cells at width 24,
/// so the table drew past its own pane. Every width is checked, because the
/// guard is worth nothing if it only covers the widths somebody thought of.
#[test]
fn the_columns_always_fit_inside_the_pane() {
    let card = engineering();
    for width in 0..=512_u16 {
        let [name, role, state, inbox, model] = columns(width, &card.members);
        assert!(
            name + role + state + inbox + model + COLUMN_SPACING <= width.max(COLUMN_SPACING),
            "columns overflow at width {width}: {name}+{role}+{state}+{inbox}+{model}"
        );
    }
}

/// A degenerate pane must produce a degenerate table, not a panic and not a
/// negative width. tmux hands out 1-cell panes during a relayout.
#[test]
fn a_one_cell_pane_produces_no_columns_rather_than_panicking() {
    let card = engineering();
    for width in [0_u16, 1, 2, 3, 4] {
        let [name, role, state, inbox, model] = columns(width, &card.members);
        assert!(name + role + state + inbox + model <= width, "width {width}");
    }
}

/// A department with nobody in it still has to answer, and the answer is a
/// table with no rows rather than an arithmetic fault.
#[test]
fn an_empty_department_allocates_no_columns_and_does_not_panic() {
    assert_eq!(columns(200, &[]), [2, 0, 0, 5, 0], "the empty table keeps its inbox header");
    assert_eq!(columns(0, &[]), [0, 0, 0, 0, 0]);
}

/// A role longer than any pane must not make the arithmetic wrap or the table
/// overflow. A roster is written by the company's own people.
#[test]
fn an_absurdly_long_role_shrinks_rather_than_overflowing() {
    let card = Card {
        name: "Desk".to_owned(),
        path: Vec::new(),
        members: vec![member("A", &"r".repeat(4000), PersonState::Working, "m", false)],
        children: Vec::new(),
    };
    let [name, role, state, inbox, model] = columns(80, &card.members);
    assert!(name + role + state + inbox + model + COLUMN_SPACING <= 80);
    assert!(role > 0, "and it still draws as much of the role as it can: {role}");
}

#[test]
fn the_inbox_column_keeps_zero_and_multi_digit_counts_right_aligned() {
    let card = engineering();
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal.draw(|frame| draw(frame, &card, true)).expect("draw");
    let buffer = terminal.backend().buffer();
    let width = usize::from(buffer.area.width);
    let rows: Vec<String> = buffer
        .content
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();
    let header = rows.iter().find(|row| row.contains("inbox")).expect("labelled inbox column");
    let inbox_byte = header.find("inbox").expect("inbox starts");
    let inbox_at = header[..inbox_byte].chars().count();
    let slice = |row: &str| row.chars().skip(inbox_at).take(5).collect::<String>();
    let ada = rows.iter().find(|row| row.contains("Ada")).expect("Ada row");
    let owen = rows.iter().find(|row| row.contains("Owen")).expect("Owen row");
    assert_eq!(slice(header), "inbox");
    assert_eq!(slice(ada), "    0", "an empty inbox is an explicit zero");
    assert_eq!(slice(owen), "   12", "counts share one right edge");
}

#[test]
fn the_inbox_column_never_truncates_the_decimal_answer() {
    let mut card = engineering();
    card.members[0].inbox_messages = 123_456;
    assert_eq!(columns(200, &card.members)[INBOX], 6);
    assert_eq!(columns(60, &card.members)[INBOX], 6, "the header gives way before a digit does");
}

#[test]
fn a_rendered_inbox_count_is_whole_or_hidden_at_its_exact_width_boundary() {
    let count = "987654";
    let card = Card {
        name: "Unit".to_owned(),
        path: Vec::new(),
        members: vec![Member {
            name: "Zed".to_owned(),
            role: String::new(),
            state: PersonState::Sleeping,
            model: String::new(),
            inbox_messages: 987_654,
            head: false,
        }],
        children: Vec::new(),
    };
    let mut first_visible = None;
    for terminal_width in 14..=24_u16 {
        let backend = TestBackend::new(terminal_width, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, &card, true)).expect("draw");
        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let member_row: String = buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .find(|row| row.contains("Zed"))
            .expect("the member row stays visible around the inbox boundary");
        let member_area_width = terminal_width.saturating_sub(4);
        let inbox_width = columns(member_area_width, &card.members)[INBOX];
        assert!(
            inbox_width == 0 || inbox_width == 6,
            "width {terminal_width} allocated a partial {inbox_width}-cell decimal"
        );
        if inbox_width == 6 {
            assert!(
                member_row.contains(count),
                "width {terminal_width} allocated the count but did not draw it whole: {member_row:?}"
            );
            first_visible.get_or_insert(terminal_width);
        } else {
            assert!(
                !count.chars().any(|digit| member_row.contains(digit)),
                "width {terminal_width} drew a clipped count: {member_row:?}"
            );
        }
    }
    let boundary = first_visible.expect("the sweep reaches a width that can show the count");
    assert_eq!(boundary, 19, "the count appears at the first frame that has all six cells");
}
