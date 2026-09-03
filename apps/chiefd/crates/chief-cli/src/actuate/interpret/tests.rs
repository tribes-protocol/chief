//! Interpreter conformance + TOCTOU tests.
//!
//! The seam: a [`RealHostExecutor`] driven by a [`ScriptedTmux`] runner is a
//! whole `HostExecutor` whose tmux answers are scripted and whose argv is
//! recorded — so the tests assert exactly what tmux was asked to do, per `Step`
//! variant, with no tmux server anywhere. Step lists are built by hand so the
//! interpreter is tested independently of the M1 planner.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::actuate::plan;
use crate::placement;

use crate::actuate::fake::{ScriptedReply, ScriptedTmux};
use crate::actuate::host::{Socket, TmuxCmd};
use crate::actuate::runner::{RecordingWaiter, SystemTmuxRunner, TmuxRunner};
use crate::actuate::spawn_cmd::LaunchSpec;
use crate::actuate::TmuxHost;
use crate::control::{ControlClient, Line};
use crate::proc::ProcReader;
use crate::real::RealHostExecutor;

use super::{
    refresh_single_ordinary_viewport_session, repair_session_rails_with,
    request_viewport_manifest_refresh, resize_session_viewport_for_attach,
    resize_session_viewport_for_client, resize_session_viewport_with,
    revoke_client_viewport_tokens, revoke_client_viewport_tokens_for_client,
    viewport_client_is_eligible, RailRepairWindow,
};
use crate::actuate::{apply_plan, StepError};

const VIEWPORT_NONCE: &str = "0123456789abcdef0123456789abcdef";
fn executor<R: TmuxRunner>(scripted: R) -> RealHostExecutor<R, RecordingWaiter> {
    RealHostExecutor::new(
        TmuxHost::new(scripted, RecordingWaiter::default()),
        ProcReader::default(),
    )
}

fn socket() -> Socket {
    Socket("chiefd-test".into())
}

/// A real tmux runner that samples one rail after every server publication.
/// Reads use the inner runner directly, so the observations cannot become
/// publications in the action under test.
struct ViewportWatchRunner {
    inner: SystemTmuxRunner,
    rail: String,
    frames: std::sync::Mutex<Vec<(i64, i64, i64, String)>>,
}

impl TmuxRunner for ViewportWatchRunner {
    fn run(
        &self,
        socket: &Socket,
        cmd: &TmuxCmd,
    ) -> Result<crate::actuate::host::TmuxOut, crate::actuate::host::HostErr> {
        let output = self.inner.run(socket, cmd)?;
        let geometry = self.inner.run(
            socket,
            &TmuxCmd {
                argv: vec![
                    "display-message".into(),
                    "-p".into(),
                    "-t".into(),
                    self.rail.clone(),
                    "-F".into(),
                    "#{window_width}\t#{window_height}\t#{pane_width}".into(),
                ],
            },
        );
        let mode = self.inner.run(
            socket,
            &TmuxCmd {
                argv: vec![
                    "show-options".into(),
                    "-w".into(),
                    "-v".into(),
                    "-t".into(),
                    self.rail.clone(),
                    "window-size".into(),
                ],
            },
        );
        if let (Ok(geometry), Ok(mode)) = (geometry, mode) {
            let values: Vec<i64> =
                geometry.stdout.trim().split('\t').filter_map(|value| value.parse().ok()).collect();
            if let [width, height, rail] = values.as_slice() {
                self.frames.lock().expect("viewport frame lock").push((
                    *width,
                    *height,
                    *rail,
                    mode.stdout.trim().to_owned(),
                ));
            }
        }
        Ok(output)
    }
}

fn real_tmux_ok(runner: &dyn TmuxRunner, socket: &Socket, argv: &[&str]) -> String {
    let output = runner
        .run(
            socket,
            &TmuxCmd { argv: argv.iter().map(|argument| (*argument).to_owned()).collect() },
        )
        .expect("tmux is installed for this real-server regression");
    assert_eq!(output.status, 0, "tmux {:?}: {}", argv, output.stderr);
    output.stdout.trim().to_owned()
}

fn launch(person: &str) -> LaunchSpec {
    LaunchSpec {
        pi_binary: std::path::PathBuf::from("/opt/pi/bin/pi"),
        pi_home: std::path::PathBuf::from(format!("/data/cobalt/.chief/agent/{person}")),
        workspace: std::path::PathBuf::from(format!("/data/cobalt/people/{person}/workspace")),
        display_name: format!("Cobalt · {person}"),
        person_name: person.to_owned(),
        accent: Some("#3c7adf".into()),
        tools: vec!["read".into()],
        extensions: Vec::new(),
        session: None,
        pending_mail: false,
        env: vec![("ORG_LAUNCHER_ORGANIZATION".into(), "cobalt".into())],
    }
}

fn launches(people: &[&str]) -> BTreeMap<String, LaunchSpec> {
    people.iter().map(|p| ((*p).to_owned(), launch(p))).collect()
}

fn desired_one_window() -> placement::Topology {
    placement::Topology {
        organization: "cobalt".into(),
        session: "cobalt-session".into(),
        windows: vec![placement::Window {
            logical_id: "eng".into(),
            name: "engineering".into(),
            panes: vec![placement::Pane {
                person_id: "vera".into(),
                launch_hash: "hash-2".into(),
                order: 0,
            }],
        }],
        known_person_ids: Default::default(),
    }
}

fn desired_two_people_one_window() -> placement::Topology {
    let mut desired = desired_one_window();
    desired.windows[0].panes.push(placement::Pane {
        person_id: "theo".into(),
        launch_hash: "hash-3".into(),
        order: 1,
    });
    desired
}

#[test]
fn manifest_refresh_places_the_epoch_before_shell_output_suppression() {
    let exec = executor(ScriptedTmux::new([ScriptedReply::ok("")]).recording_viewport_authority());
    request_viewport_manifest_refresh(&exec, &socket(), "org-cobalt_", "73");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 1);
    let command = calls[0]
        .iter()
        .find(|argument| {
            argument.contains("run-shell") && argument.contains("@chief_viewport_refresh_command")
        })
        .expect("hidden manifest refresh command");
    let epoch = command.find(" 73 ").expect("epoch is a Chief argument");
    let suppression = command.find(">/dev/null").expect("callback output is suppressed");
    assert!(epoch < suppression, "generation must precede shell suppression: {command}");
    assert!(command.ends_with("|| :'"), "the complete hidden job is silent: {command}");
}

fn empty_observed(session_exists: bool) -> plan::ObservedTopology {
    plan::ObservedTopology {
        session_exists,
        session_organization: if session_exists { "cobalt".into() } else { String::new() },
        windows: Vec::new(),
        panes: Vec::new(),
    }
}

#[test]
fn missing_sidebar_is_repaired_with_bare_chief_in_the_company_directory() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("@1\t%1\t1\n@2\t%2"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok("%9"),
        ScriptedReply::ok(""),
    ])
    .recording_viewport_authority();
    let exec = executor(scripted);

    let repaired = repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("the missing rail is repaired");

    assert_eq!(repaired, 1);
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(
        calls.len(),
        6,
        "survey, preferences, fence, ONE split+tag, and final refresh: {calls:?}"
    );
    // THE SPLIT AND THE TAG ARE ONE INVOCATION, and this is a correctness rule
    // rather than a thrift about round trips. Sent as two, the rail's own
    // `chief sidebar` is running and attached to the brain while its pane is
    // still untagged, and every rail count in this file keys on the TAG — so a
    // concurrent survey reads "this window lost its sidebar" and splits a
    // second rail into it. That is how an operator's window came to draw the
    // company twice, once down each edge. tmux's command loop runs a batch back
    // to back, so the untagged state is never observable.
    assert_eq!(
        calls[4],
        vec![
            "split-window",
            "-h",
            "-b",
            "-l",
            "26",
            "-t",
            "%2",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            "/data/cobalt",
            "/opt/chief/bin/chief",
            "sidebar",
            ";",
            "set-option",
            "-p",
            "-t",
            "@2",
            "@organization_sidebar",
            "1",
            ";",
            // And the batch ends by handing the cursor back to the pane the
            // split took it from, so a repair never moves the operator into
            // the sidebar. It is last because the tag above names a WINDOW.
            "select-pane",
            "-l",
            "-t",
            "@2",
        ],
        "the current sidebar verb takes no company argument and resolves the company from cwd, \
         and the pane is tagged in the same command sequence that creates it"
    );
    assert!(calls[3].iter().any(|arg| arg.contains("@chief_viewport_topology_epoch")));
    assert!(calls[5].iter().any(|arg| arg.contains("@chief_viewport_refresh_command")));
}

/// THE REGRESSION, AT THE SEAM THAT CAUSED IT.
///
/// An operator's window rendered the company twice — one rail pinned left, one
/// pinned right, the same tree in both — and the guard that swears a managed
/// window has exactly one rail never said a word, because it counts panes
/// carrying `@organization_sidebar` and the duplicate carried nothing. The
/// duplicate was born here: this sweep split a rail, and a second invocation
/// tagged it. Between those two invocations the pane existed, its
/// `chief sidebar` process was up and painting, and it was untagged — so a
/// concurrent sweep counted zero rails in that window and split another.
///
/// The claim under test is therefore not "a tag is set" but "no observer can
/// ever see this pane untagged": the tag must ride in the SAME tmux invocation
/// as the split. Split the batch in two again and this fails.
#[test]
fn a_repaired_rail_is_never_observable_before_it_carries_its_tag() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("@1\t%1\t1\n@2\t%2"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok("%9"),
        ScriptedReply::ok(""),
    ])
    .recording_viewport_authority();
    let exec = executor(scripted);

    repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("the missing rail is repaired");

    let calls = exec.tmux_host().runner().calls();
    let splits: Vec<&Vec<String>> = calls
        .iter()
        .filter(|call| call.first().is_some_and(|verb| verb == "split-window"))
        .collect();
    assert_eq!(splits.len(), 1, "one rail is minted, not two: {calls:?}");
    assert!(
        splits[0].iter().any(|arg| arg == "@organization_sidebar"),
        "the tag rides in the same invocation as the split, so the pane is never observable \
         untagged — an untagged rail is one no guard in this file can count: {:?}",
        splits[0]
    );
    assert!(
        !calls.iter().any(|call| {
            call.first().is_some_and(|verb| verb == "set-option")
                && call.iter().any(|arg| arg == "@organization_sidebar")
        }),
        "and NO standalone tagging invocation survives; a separate one is exactly the gap a \
         concurrent sweep splits a second rail into: {calls:?}"
    );
}

/// A REPAIR PASS DOES NOT TAKE THE CURSOR AWAY FROM THE PERSON.
///
/// The mint batch has no `-d`, so the new rail is active for the tag that
/// follows it. The batch therefore ends by giving the cursor back with
/// `select-pane -l` — the window's last pane, which the split makes the pane
/// that was active before it. Without it, a rail repaired under an operator
/// who is typing moves them into the sidebar mid-sentence.
#[test]
fn a_repaired_rail_gives_the_cursor_back_after_it_is_tagged() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("@1\t%1\t1\n@2\t%2"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok("%9"),
        ScriptedReply::ok(""),
    ])
    .recording_viewport_authority();
    let exec = executor(scripted);

    repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("the missing rail is repaired");

    let calls = exec.tmux_host().runner().calls();
    let split = calls
        .iter()
        .find(|call| call.first().is_some_and(|verb| verb == "split-window"))
        .expect("the mint batch");
    let commands: Vec<&[String]> = split.split(|arg| arg == ";").collect();
    assert_eq!(
        commands.last().expect("the batch is not empty"),
        &["select-pane".to_owned(), "-l".to_owned(), "-t".to_owned(), "@2".to_owned()],
        "the batch ends by restoring the pane the split took the cursor from: {split:?}"
    );
    let tag = commands
        .iter()
        .position(|command| command.iter().any(|arg| arg == "@organization_sidebar"))
        .expect("the tag");
    assert!(
        tag < commands.len() - 1,
        "the tag names a WINDOW, so it must land while the rail is still active: {split:?}"
    );
}

#[test]
fn failed_sidebar_split_still_refreshes_after_the_epoch_fence() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("@1\t%1\t1\n@2\t%2"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::failed("split refused"),
    ])
    .recording_viewport_authority();
    let exec = executor(scripted);
    let error = repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect_err("split refusal fails the sweep");
    assert!(error.contains("split refused"));
    let calls = exec.tmux_host().runner().calls();
    let invalidate = calls
        .iter()
        .position(|call| call.iter().any(|arg| arg.contains("@chief_viewport_topology_epoch")))
        .expect("epoch fence");
    let split = calls
        .iter()
        .position(|call| call.first().is_some_and(|verb| verb == "split-window"))
        .expect("split attempt");
    let refresh = calls
        .iter()
        .position(|call| call.iter().any(|arg| arg.contains("@chief_viewport_refresh_command")))
        .expect("final refresh attempt");
    assert!(invalidate < split && split < refresh, "calls: {calls:?}");
}

#[test]
fn actuator_repair_mints_at_the_sessions_custom_expanded_width() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("@1\t%1\t1\n@2\t%2"),
        ScriptedReply::ok("0"),
        ScriptedReply::ok("37"),
        ScriptedReply::ok("%9"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("repair");
    let calls = exec.tmux_host().runner().calls();
    assert!(calls.iter().any(|call| { call.windows(2).any(|pair| pair == ["-l", "37"]) }));
    assert!(!calls.iter().any(|call| {
        call.first().is_some_and(|verb| verb == "set-option")
            && call.iter().any(|arg| arg == "@chief_sidebar_columns")
    }));
}

#[test]
fn complete_sidebar_set_is_steady_state_even_when_topology_has_no_work() {
    let scripted = ScriptedTmux::new([ScriptedReply::ok("@1\t%1\t1\n@2\t%2\t1")]);
    let exec = executor(scripted);

    let repaired = repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("a complete rail set is readable");

    assert_eq!(repaired, 0);
    assert_eq!(exec.tmux_host().runner().calls().len(), 1, "the survey is the only call");
}

#[test]
fn rail_sweep_repairs_a_166_column_window_from_the_active_240_column_window() {
    let active_layout = crate::layout::organization_tmux_layout(
        240,
        55,
        Some(crate::layout::Rail { pane_id: "%1", columns: 31 }),
        &["%2"],
    )
    .expect("active layout");
    let survey = format!(
        "@1\t%1\t1\t240\t55\t31\t0\t{active_layout}\tmanual\texecutive\n\
         @1\t%2\t\t240\t55\t31\t0\t{active_layout}\tmanual\texecutive\n\
         @2\t%3\t1\t166\t42\t31\t0\twrong-layout\tlatest\tmarket\n\
         @2\t%4\t\t166\t42\t31\t0\twrong-layout\tlatest\tmarket"
    );
    let scripted = ScriptedTmux::new([ScriptedReply::ok(&survey), ScriptedReply::ok("")])
        .with_geometry([ScriptedReply::ok("240\t55\texecutive")]);
    let exec = executor(scripted);

    let repaired = repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("the geometry sweep succeeds");

    assert_eq!(repaired, 0, "both rails already exist");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.iter().filter(|call| call[0] == "resize-window").count(), 1);
    let repair = calls.iter().find(|call| call[0] == "resize-window").expect("repair");
    let layout = repair.iter().position(|word| word == "select-layout");
    let ownership = repair.iter().position(|word| word == "window-size");
    assert!(
        layout.zip(ownership).is_some_and(|(layout, ownership)| layout < ownership),
        "the canonical resize, final layout, and manual ownership are one publication: {repair:?}"
    );
    assert!(
        repair.iter().any(|word| word.contains("31x55")),
        "the final layout keeps the session's effective custom rail width: {repair:?}"
    );
    assert!(
        !calls.iter().any(|call| {
            call.first().is_some_and(|verb| verb == "set-option")
                && call.iter().any(|word| word == "@chief_sidebar_columns")
        }),
        "viewport repair reads the preference but never writes it: {calls:?}"
    );
    assert!(!calls.iter().any(|call| call[0] == "split-window"), "no duplicate rail");
}

#[test]
fn active_viewport_with_matching_geometry_still_repairs_its_wrong_rail_layout() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(
            "@1\t%1\t1\t348\t59\t26\twrong-layout\n\
             @1\t%2\t\t348\t59\t26\twrong-layout",
        ),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
    ])
    .with_geometry([ScriptedReply::ok("348\t59\texecutive")]);
    let exec = executor(scripted);

    repair_session_rails_with(
        &exec,
        &socket(),
        "cobalt-session",
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("active viewport repair");

    let calls = exec.tmux_host().runner().calls();
    let repair = calls
        .iter()
        .find(|call| call.first().is_some_and(|verb| verb == "select-layout"))
        .expect("matching geometry with a wrong layout must still publish the final layout");
    assert!(repair.iter().any(|word| word.contains("26x59")), "effective rail: {repair:?}");
    assert!(repair.iter().any(|word| word == "manual"), "manual ownership: {repair:?}");
}

#[test]
fn viewport_layout_keeps_collapsed_and_custom_sidebar_widths() {
    for columns in [4, 31] {
        let window = RailRepairWindow {
            current: None,
            layout: None,
            panes: vec!["%3".to_owned(), "%4".to_owned()],
            rails: vec!["%3".to_owned()],
            columns: Some(columns),
            ..RailRepairWindow::default()
        };
        let layout = window
            .final_layout(crate::window_geometry::Geometry { columns: 240, rows: 56 })
            .expect("the two-pane layout is valid")
            .expect("the rail has a body beside it");
        assert!(
            layout.contains(&format!("{columns}x56,0,0,3")),
            "viewport repair keeps the effective width {columns}: {layout}"
        );
    }
}

#[test]
fn viewport_callback_validates_then_publishes_all_managed_windows_once() {
    let survey = "@1\t%1\t1\t240\t56\t31\t0\told-one\tmanual\texecutive\n\
                  @1\t%2\t\t240\t56\t31\t0\told-one\tmanual\texecutive\n\
                  @2\t%3\t1\t240\t56\t31\t0\told-two\tmanual\tmarket\n\
                  @2\t%4\t\t240\t56\t31\t0\told-two\tmanual\tmarket";
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("cobalt\t23\t23"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);

    assert_eq!(
        resize_session_viewport_with(
            &exec,
            &socket(),
            "org-cobalt_",
            crate::window_geometry::Geometry { columns: 360, rows: 84 },
        )
        .expect("viewport publication"),
        2
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 3, "one tag read, one snapshot, one final publication: {calls:?}");
    let publish = &calls[2];
    // EVERY WINDOW'S PUBLICATION CARRIES THE CENSUS IT WAS COMPUTED FROM. An
    // absolute layout string enumerates a whole window, so it is appliable only
    // to the pane set it was built against — and it is applied one tmux
    // invocation after that set was read. Both windows here held two panes.
    assert_eq!(publish.iter().filter(|word| *word == "if-shell").count(), 2);
    assert_eq!(
        publish.iter().filter(|word| word.as_str() == "#{==:#{window_panes},2}").count(),
        2,
        "one pane-census fence per managed window: {publish:?}"
    );
    let bodies: Vec<&String> =
        publish.iter().filter(|word| word.contains("select-layout")).collect();
    assert_eq!(bodies.len(), 2, "one guarded body per window: {publish:?}");
    for body in &bodies {
        assert!(body.contains("resize-window"), "{body}");
        assert!(body.contains("manual"), "{body}");
        assert!(body.contains("31x84"), "custom rail: {body}");
    }
    assert!(!publish.iter().any(|word| word == "@chief_sidebar_columns"));
    assert!(!publish.iter().any(|word| word == "@chief_sidebar_collapsed"));
}

#[test]
fn viewport_callback_uses_collapsed_width_without_changing_the_expanded_preference() {
    let survey = "@1\t%1\t1\t240\t56\t37\t1\told\tmanual\texecutive\n\
                  @1\t%2\t\t240\t56\t37\t1\told\tmanual\texecutive";
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("cobalt\t23\t23"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    resize_session_viewport_with(
        &exec,
        &socket(),
        "org-cobalt_",
        crate::window_geometry::Geometry { columns: 360, rows: 84 },
    )
    .expect("collapsed viewport publication");
    let calls = exec.tmux_host().runner().calls();
    assert!(calls[2].iter().any(|word| word.contains("4x84")), "collapsed rail: {calls:?}");
    assert!(!calls[2].iter().any(|word| word == "37"), "preference is not written: {calls:?}");
}

#[test]
fn viewport_callback_refuses_an_invalid_managed_window_before_mutation() {
    let survey = "@1\t%1\t\t240\t56\t26\t0\told\tmanual\texecutive";
    let scripted = ScriptedTmux::new([ScriptedReply::ok("cobalt"), ScriptedReply::ok(survey)]);
    let exec = executor(scripted);
    let error = resize_session_viewport_with(
        &exec,
        &socket(),
        "org-cobalt_",
        crate::window_geometry::Geometry { columns: 360, rows: 84 },
    )
    .expect_err("a managed window without one rail fails closed");
    assert!(error.contains("exactly one sidebar rail"), "{error}");
    assert_eq!(exec.tmux_host().runner().calls().len(), 2, "validation is read-only");
}

#[test]
fn viewport_callback_refuses_an_untagged_session_before_listing_panes() {
    let scripted = ScriptedTmux::new([ScriptedReply::ok("")]);
    let exec = executor(scripted);
    let error = resize_session_viewport_with(
        &exec,
        &socket(),
        "org-lookalike_",
        crate::window_geometry::Geometry { columns: 360, rows: 84 },
    )
    .expect_err("a session name alone grants no geometry authority");
    assert!(error.contains("not tagged as a Chief company session"), "{error}");
    assert_eq!(exec.tmux_host().runner().calls().len(), 1);
}

#[test]
fn viewport_callback_resizes_a_rail_only_department_beside_furnished_windows() {
    // Exact live shape from the operator after Chief moved into its focus window: the
    // Executive window keeps its managed window tag and rail, but has no body
    // pane. One empty department must not stop the other managed windows from
    // following the browser viewport.
    let survey = "@1\t%1\t1\t133\t47\t\t0\told-executive\tmanual\texecutive\n\
                  @2\t%2\t1\t133\t47\t\t0\told-focus\tmanual\t__focus__\n\
                  @2\t%3\t\t133\t47\t\t0\told-focus\tmanual\t__focus__";
    let exec = executor(ScriptedTmux::new([
        ScriptedReply::ok("zipbox-ai"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok(""),
    ]));

    assert_eq!(
        resize_session_viewport_with(
            &exec,
            &socket(),
            "org-zipbox-ai_",
            crate::window_geometry::Geometry { columns: 145, rows: 47 },
        )
        .expect("a rail-only department is valid managed furniture"),
        2,
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 3);
    let publication = &calls[2];
    let guarded = |window: &str, panes: &str| -> String {
        let at = publication
            .iter()
            .position(|word| word == window)
            .unwrap_or_else(|| panic!("{window} is published: {publication:?}"));
        assert_eq!(publication[at - 3], "if-shell", "{publication:?}");
        assert_eq!(publication[at + 1], format!("#{{==:#{{window_panes}},{panes}}}"));
        publication[at + 2].clone()
    };
    // The rail-only Executive department is one pane; the furnished focus
    // window is two. Each carries its own census, so a person arriving in one
    // of them cannot make the other's publication unappliable.
    let executive = guarded("@1", "1");
    let focus = guarded("@2", "2");
    assert!(executive.contains("resize-window"), "{executive}");
    assert!(focus.contains("resize-window"), "{focus}");
    assert!(
        !executive.contains("select-layout"),
        "a rail-only department has no split to select: {executive}"
    );
    assert!(focus.contains("select-layout"), "the furnished window is laid out: {focus}");
}

#[test]
fn attach_viewport_resize_keeps_every_mutation_behind_server_and_topology_authority() {
    let survey = "@1\t%1\t1\t240\t56\t26\t0\told\tmanual\texecutive\n\
                  @1\t%2\t\t240\t56\t26\t0\told\tmanual\texecutive";
    let exec = executor(ScriptedTmux::new([
        ScriptedReply::ok("cobalt"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok("stale"),
    ]));
    let outcome = resize_session_viewport_for_attach(
        &exec,
        &socket(),
        "org-cobalt_",
        "cobalt",
        7,
        VIEWPORT_NONCE,
        (360, 84),
    )
    .expect("a stale authority is a clean compare-and-swap loss");
    assert_eq!(outcome, super::AttachViewportPublication::Stale);
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2][0..4], ["if-shell", "-F", "-t", "org-cobalt_"]);
    assert!(calls[2][4].contains("#{==:#{@organization_id},cobalt}"));
    assert!(calls[2][4].contains("#{==:#{@chief_viewport_topology_epoch},7}"));
    assert!(calls[2][4].contains(VIEWPORT_NONCE));
    assert!(calls[2][5].contains("resize-window"));
}

#[test]
fn client_viewport_callback_uses_c_and_guards_the_publication_with_the_exact_event() {
    let survey = "@1\t%1\t1\t240\t56\t26\t0\told\tmanual\texecutive\n\
                  @1\t%2\t\t240\t56\t26\t0\told\tmanual\texecutive";
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/9"),
        ScriptedReply::ok("cobalt"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok("applied"),
    ]);
    let exec = executor(scripted);

    assert_eq!(
        resize_session_viewport_for_client(
            &exec,
            &socket(),
            "org-cobalt_",
            "cobalt",
            "/dev/pts/9",
            "41",
            VIEWPORT_NONCE,
        )
        .expect("current client event publishes"),
        1
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls[0][0..4], ["display-message", "-p", "-c", "/dev/pts/9"]);
    assert_eq!(calls[3][0..4], ["if-shell", "-t", "org-cobalt_", "-F"]);
    assert_eq!(
        calls[3][4],
        "#{&&:#{==:#{@organization_id},cobalt},#{&&:#{==:#{@chief_viewport_request},41},#{&&:#{==:#{@chief_viewport_owner},/dev/pts/9},#{==:#{@chief_viewport_server_nonce},0123456789abcdef0123456789abcdef}}}}"
    );
    assert!(calls[3][5].contains("resize-window"), "guarded final publication: {:?}", calls[3]);
}

#[test]
fn client_viewport_callback_refuses_detach_switch_and_name_reuse_then_clears_its_request() {
    for (reply, expected) in [
        (ScriptedReply::failed("can't find client"), "no longer present"),
        (
            ScriptedReply::ok("org-other_\t360\t84\tattached,focused\t41\t/dev/pts/reused"),
            "belongs to org-other_",
        ),
        (
            ScriptedReply::ok("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/other"),
            "target is stale or was reused",
        ),
    ] {
        let exec = executor(ScriptedTmux::new([reply, ScriptedReply::ok("")]));
        let error = resize_session_viewport_for_client(
            &exec,
            &socket(),
            "org-cobalt_",
            "cobalt",
            "/dev/pts/reused",
            "41",
            VIEWPORT_NONCE,
        )
        .expect_err("stale client authority must fail closed");
        assert!(error.contains(expected), "{error}");
        let calls = exec.tmux_host().runner().calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1][0..4], ["if-shell", "-t", "org-cobalt_", "-F"]);
        assert_eq!(
            calls[1][4],
            "#{&&:#{==:#{@organization_id},cobalt},#{&&:#{==:#{@chief_viewport_request},41},#{&&:#{==:#{@chief_viewport_owner},/dev/pts/reused},#{==:#{@chief_viewport_server_nonce},0123456789abcdef0123456789abcdef}}}}"
        );
        assert!(calls[1][5].contains("@chief_viewport_owner"));
    }
}

#[test]
fn client_viewport_callback_rejects_a_non_numeric_request_without_tmux_input() {
    let exec = executor(ScriptedTmux::new([]));
    let error = resize_session_viewport_for_client(
        &exec,
        &socket(),
        "org-cobalt_",
        "cobalt",
        "/dev/pts/9",
        "#{@hostile}",
        VIEWPORT_NONCE,
    )
    .expect_err("only a hook-minted numeric generation is accepted");
    assert!(error.contains("must be numeric"), "{error}");
    assert!(exec.tmux_host().runner().calls().is_empty());
}

#[test]
fn viewport_hook_eligibility_accepts_only_one_exact_ordinary_sized_company_client() {
    for (reply, tag, expected, expected_calls) in [
        ("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/9", "cobalt", true, 2),
        ("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/9", "", false, 2),
        ("org-other_\t360\t84\tattached,focused\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t\t84\tattached,focused\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t360\t\tattached,focused\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t0\t84\tattached,focused\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t360\t0\tattached,focused\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t360\t84\tcontrol-mode\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t360\t84\tignore-size\t41\t/dev/pts/9", "", false, 1),
        ("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/reused", "", false, 1),
    ] {
        let tagged = if tag.is_empty() {
            format!("\t{VIEWPORT_NONCE}")
        } else {
            format!("{tag}\t{VIEWPORT_NONCE}")
        };
        let exec =
            executor(ScriptedTmux::new([ScriptedReply::ok(reply), ScriptedReply::ok(&tagged)]));
        assert_eq!(
            viewport_client_is_eligible(
                &exec,
                &socket(),
                "org-cobalt_",
                "/dev/pts/9",
                VIEWPORT_NONCE,
            )
            .expect("the exact client probe answers"),
            expected,
            "eligibility for {reply:?}"
        );
        let calls = exec.tmux_host().runner().calls();
        assert_eq!(calls.len(), expected_calls);
        assert_eq!(calls[0][0..4], ["display-message", "-p", "-c", "/dev/pts/9"]);
        if expected_calls == 2 {
            assert_eq!(calls[1][0..4], ["display-message", "-p", "-t", "org-cobalt_"]);
        }
    }

    let exec = executor(ScriptedTmux::new([ScriptedReply::failed("can't find client")]));
    let error =
        viewport_client_is_eligible(&exec, &socket(), "org-cobalt_", "/dev/pts/9", VIEWPORT_NONCE)
            .expect_err("a missing hook client is ineligible and loud to the internal caller");
    assert!(error.contains("no longer present"), "{error}");
    assert_eq!(exec.tmux_host().runner().calls().len(), 1);
}

#[test]
fn viewport_membership_census_publishes_only_one_ordinary_tagged_session_under_cas() {
    let exec = executor(ScriptedTmux::new([
        ScriptedReply::ok(
            "/dev/pts/9\torg-cobalt_\t360\t84\tattached,focused\ncontrol\torg-cobalt_\t\t\tcontrol-mode,ignore-size",
        ),
        ScriptedReply::ok("cobalt\t23\t23"),
        ScriptedReply::ok(""),
    ]));
    refresh_single_ordinary_viewport_session(&exec, &socket(), "17", VIEWPORT_NONCE)
        .expect("one exact ordinary company client owns the fast path");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0][0..2], ["list-clients", "-F"]);
    assert_eq!(
        calls[1],
        [
            "display-message",
            "-p",
            "-t",
            "org-cobalt_",
            "#{@organization_id}\t#{@chief_viewport_topology_epoch}\t#{@chief_viewport_manifest_epoch}",
        ]
    );
    assert_eq!(calls[2][0..2], ["if-shell", "-F"]);
    assert!(calls[2][2].contains("#{==:#{@chief_viewport_membership_generation},17}"));
    assert!(calls[2][2].contains(VIEWPORT_NONCE));
    assert!(calls[2][3].contains("set-option -g @chief_viewport_fast_session org-cobalt_"));
    assert!(calls[2][3].contains("set-option -g @chief_viewport_fast_owner /dev/pts/9"));
    assert!(calls[2][3].contains("set-option -g @chief_viewport_fast_organization cobalt"));
    assert!(calls[2][3].contains("set-option -g @chief_viewport_fast_generation 17"));

    let exec = executor(ScriptedTmux::new([
        ScriptedReply::ok(
            "/dev/pts/9\torg-cobalt_\t360\t84\tattached\n/dev/pts/10\torg-cobalt_\t240\t56\tattached,focused",
        ),
        ScriptedReply::ok(""),
    ]));
    refresh_single_ordinary_viewport_session(&exec, &socket(), "18", VIEWPORT_NONCE)
        .expect("two ordinary clients disable the native path");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1][3].contains("set-option -gu @chief_viewport_fast_session"));
    assert!(calls[1][3].contains("set-option -gu @chief_viewport_fast_owner"));
}

#[test]
fn viewport_membership_census_rejects_non_numeric_generation_without_tmux_input() {
    let exec = executor(ScriptedTmux::new([]));
    let error =
        refresh_single_ordinary_viewport_session(&exec, &socket(), "#{hostile}", VIEWPORT_NONCE)
            .expect_err("only a hook-minted numeric generation can publish membership");
    assert!(error.contains("must be numeric"), "{error}");
    assert!(exec.tmux_host().runner().calls().is_empty());
}

#[test]
fn client_viewport_callback_cannot_clean_options_from_an_untagged_lookalike_session() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("org-lookalike_\t360\t84\tattached,focused\t41\t/dev/pts/9"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let error = resize_session_viewport_for_client(
        &exec,
        &socket(),
        "org-lookalike_",
        "cobalt",
        "/dev/pts/9",
        "41",
        VIEWPORT_NONCE,
    )
    .expect_err("a session name is not company authority");
    assert!(error.contains("not tagged as a Chief company session"), "{error}");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2][0..4], ["if-shell", "-t", "org-lookalike_", "-F"]);
    assert_eq!(
        calls[2][4],
        "#{&&:#{==:#{@organization_id},cobalt},#{&&:#{==:#{@chief_viewport_request},41},#{&&:#{==:#{@chief_viewport_owner},/dev/pts/9},#{==:#{@chief_viewport_server_nonce},0123456789abcdef0123456789abcdef}}}}"
    );
    assert!(
        calls[2][5].contains("@chief_viewport_owner"),
        "the unset exists only in the guarded true branch: {:?}",
        calls[2]
    );
}

#[test]
fn client_viewport_callback_refuses_a_recreated_session_with_the_wrong_organization() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/9"),
        ScriptedReply::ok("amber"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let error = resize_session_viewport_for_client(
        &exec,
        &socket(),
        "org-cobalt_",
        "cobalt",
        "/dev/pts/9",
        "41",
        VIEWPORT_NONCE,
    )
    .expect_err("a reused session name does not preserve organization authority");
    assert!(error.contains("belongs to amber, not cobalt"), "{error}");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 3, "the wrong organization is never surveyed or mutated");
    assert_eq!(
        calls[2][4],
        "#{&&:#{==:#{@organization_id},cobalt},#{&&:#{==:#{@chief_viewport_request},41},#{&&:#{==:#{@chief_viewport_owner},/dev/pts/9},#{==:#{@chief_viewport_server_nonce},0123456789abcdef0123456789abcdef}}}}"
    );
}

#[test]
fn client_viewport_callback_owner_mismatch_has_no_publication_or_cleanup_authority() {
    let survey = "@1\t%1\t1\t240\t56\t26\t0\told\tmanual\texecutive\n\
                  @1\t%2\t\t240\t56\t26\t0\told\tmanual\texecutive";
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/9"),
        ScriptedReply::ok("cobalt"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok("stale"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let error = resize_session_viewport_for_client(
        &exec,
        &socket(),
        "org-cobalt_",
        "cobalt",
        "/dev/pts/9",
        "41",
        VIEWPORT_NONCE,
    )
    .expect_err("an owner mismatch makes both CAS operations stale");
    assert!(error.contains("became stale before publication"), "{error}");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls[3][4], calls[4][4], "publication and cleanup use one exact guard");
    assert!(calls[3][4].contains("#{==:#{@chief_viewport_owner},/dev/pts/9}"));
}

#[test]
fn client_viewport_callback_refuses_control_and_ignore_size_clients() {
    for flags in ["control-mode,focused", "attached,ignore-size"] {
        let exec = executor(ScriptedTmux::new([
            ScriptedReply::ok(&format!("org-cobalt_\t360\t84\t{flags}\t41\t/dev/pts/9")),
            ScriptedReply::ok(""),
        ]));
        assert_eq!(
            resize_session_viewport_for_client(
                &exec,
                &socket(),
                "org-cobalt_",
                "cobalt",
                "/dev/pts/9",
                "41",
                VIEWPORT_NONCE,
            )
            .expect("non-authoritative clients are ignored"),
            0
        );
        let calls = exec.tmux_host().runner().calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[1][4],
            "#{&&:#{==:#{@organization_id},cobalt},#{&&:#{==:#{@chief_viewport_request},41},#{&&:#{==:#{@chief_viewport_owner},/dev/pts/9},#{==:#{@chief_viewport_server_nonce},0123456789abcdef0123456789abcdef}}}}"
        );
    }
}

#[test]
fn client_viewport_callback_refuses_an_event_superseded_at_the_mutation_boundary() {
    let survey = "@1\t%1\t1\t240\t56\t26\t0\told\tmanual\texecutive\n\
                  @1\t%2\t\t240\t56\t26\t0\told\tmanual\texecutive";
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("org-cobalt_\t360\t84\tattached,focused\t41\t/dev/pts/9"),
        ScriptedReply::ok("cobalt"),
        ScriptedReply::ok(survey),
        ScriptedReply::ok("stale"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let error = resize_session_viewport_for_client(
        &exec,
        &socket(),
        "org-cobalt_",
        "cobalt",
        "/dev/pts/9",
        "41",
        VIEWPORT_NONCE,
    )
    .expect_err("a newer hook event cancels the old publication");
    assert!(error.contains("became stale before publication"), "{error}");
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[3][0], "if-shell", "the mutation stays behind the atomic guard");
    assert_eq!(
        calls[4][4],
        "#{&&:#{==:#{@organization_id},cobalt},#{&&:#{==:#{@chief_viewport_request},41},#{&&:#{==:#{@chief_viewport_owner},/dev/pts/9},#{==:#{@chief_viewport_server_nonce},0123456789abcdef0123456789abcdef}}}}"
    );
}

#[test]
fn a_gone_client_revokes_its_owner_from_every_tagged_company() {
    let sessions = "org-old_\tcobalt\t/dev/pts/9\t41\t7\n\
                    org-other_\tamber\t/dev/pts/9\t42\t8\n\
                    org-safe_\tquartz\t/dev/pts/8\t43\t9";
    let scripted = ScriptedTmux::new([
        ScriptedReply::failed("can't find client"),
        ScriptedReply::ok(sessions),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    assert_eq!(
        revoke_client_viewport_tokens_for_client(&exec, &socket(), "/dev/pts/9", VIEWPORT_NONCE)
            .expect("a detached session-change callback still revokes its owner"),
        2
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls[0][0..4], ["display-message", "-p", "-c", "/dev/pts/9"]);
    assert_eq!(calls[1][0], "list-sessions");
    assert!(calls[2].windows(2).any(|words| words == ["-t", "org-old_"]));
    assert!(calls[2].windows(2).any(|words| words == ["-t", "org-other_"]));
    assert!(!calls[2].iter().any(|word| word == "org-safe_"));
}

#[test]
fn session_change_revokes_only_this_clients_tokens_in_old_tagged_companies() {
    let sessions = "org-old_\tcobalt\t/dev/pts/9\t41\t7\n\
                    org-new_\tamber\t/dev/pts/9\t42\t8\n\
                    org-other_\tquartz\t/dev/pts/7\t43\t9\n\
                    scratch\t\t/dev/pts/9\t44\t10";
    let scripted = ScriptedTmux::new([ScriptedReply::ok(sessions), ScriptedReply::ok("")]);
    let exec = executor(scripted);
    assert_eq!(
        revoke_client_viewport_tokens(&exec, &socket(), "/dev/pts/9", "org-new_", VIEWPORT_NONCE,)
            .expect("session-change revocation"),
        1
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1][0..4], ["if-shell", "-F", "-t", "org-old_"]);
    assert!(calls[1][4].contains("#{==:#{@organization_id},cobalt}"));
    assert!(calls[1][4].contains("#{==:#{@chief_viewport_request},41}"));
    assert!(calls[1][4].contains("#{==:#{@chief_viewport_topology_epoch},7}"));
    assert!(calls[1][4].contains(VIEWPORT_NONCE));
}

#[test]
fn session_change_refuses_hostile_targets_and_clients_without_a_mutation() {
    let sessions = "org-safe_\tcobalt\t/dev/pts/9\t41\t7\n\
                    org-x_; kill-server\tcobalt\t/dev/pts/9\t42\t8\n\
                    org-other_\tamber,#{pane_id}\t/dev/pts/9\t43\t9";
    let exec = executor(ScriptedTmux::new([ScriptedReply::ok(sessions), ScriptedReply::ok("")]));
    assert_eq!(
        revoke_client_viewport_tokens(&exec, &socket(), "/dev/pts/9", "", VIEWPORT_NONCE)
            .expect("only the safe company target is considered"),
        1
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].iter().any(|word| word == "org-safe_"));
    assert!(!calls[1].iter().any(|word| word.contains("kill-server")));
    assert!(!calls[1].iter().any(|word| word.contains("amber")));

    let exec = executor(ScriptedTmux::new([]));
    let error = revoke_client_viewport_tokens(&exec, &socket(), "#{},", "", VIEWPORT_NONCE)
        .expect_err("hostile client format text is refused before tmux");
    assert!(error.contains("not safe tmux format text"), "{error}");
    assert!(exec.tmux_host().runner().calls().is_empty());
}

#[test]
fn session_change_clear_is_fenced_by_the_exact_listed_token_and_topology() {
    let sessions = "org-old_\tcobalt\t/dev/pts/9\t41\t7";
    let exec = executor(ScriptedTmux::new([ScriptedReply::ok(sessions), ScriptedReply::ok("")]));
    assert_eq!(
        revoke_client_viewport_tokens(&exec, &socket(), "/dev/pts/9", "", VIEWPORT_NONCE)
            .expect("the final CAS can lose without clearing a newer token"),
        1
    );
    let calls = exec.tmux_host().runner().calls();
    let predicate = &calls[1][4];
    assert!(predicate.contains("#{==:#{@organization_id},cobalt}"));
    assert!(predicate.contains("#{==:#{@chief_viewport_request},41}"));
    assert!(predicate.contains("#{==:#{@chief_viewport_owner},/dev/pts/9}"));
    assert!(predicate.contains("#{==:#{@chief_viewport_topology_epoch},7}"));
    assert!(predicate.contains(VIEWPORT_NONCE));
    assert!(calls[1][5].contains("'org-old_'"), "clear argv is quoted: {:?}", calls[1]);
}

/// The exact 348x62 -> 240x56 browser-resize failure against real tmux.
///
/// tmux 3.3a gives all shrinkage to the first split until it reaches one
/// column: a correct 31-column rail therefore becomes one column when
/// `resize-window` publishes by itself. The repair must apply the final layout
/// in that same server sequence, so every sampled command boundary stays at the
/// session's effective custom width.
#[test]
fn real_viewport_repair_never_publishes_a_one_column_rail() {
    let raw = SystemTmuxRunner::default();
    let socket = Socket(format!("chiefd-viewport-frame-{}", std::process::id()));
    let session = format!("chiefd-viewport-frame-{}", std::process::id());
    real_tmux_ok(
        &raw,
        &socket,
        &["new-session", "-d", "-s", &session, "-x", "240", "-y", "56", "sleep", "120"],
    );
    real_tmux_ok(
        &raw,
        &socket,
        &["set-option", "-w", "-t", &session, "@organization_window_id", "executive"],
    );
    real_tmux_ok(&raw, &socket, &["set-option", "-t", &session, "@organization_id", "cobalt"]);
    let active_rail = real_tmux_ok(
        &raw,
        &socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            "31",
            "-t",
            &session,
            "-P",
            "-F",
            "#{pane_id}",
            "sleep",
            "120",
        ],
    );
    real_tmux_ok(
        &raw,
        &socket,
        &["set-option", "-p", "-t", &active_rail, "@organization_sidebar", "1"],
    );
    let hidden = real_tmux_ok(
        &raw,
        &socket,
        &["new-window", "-d", "-t", &session, "-P", "-F", "#{window_id}", "sleep", "120"],
    );
    real_tmux_ok(
        &raw,
        &socket,
        &["set-option", "-w", "-t", &hidden, "@organization_window_id", "market"],
    );
    real_tmux_ok(&raw, &socket, &["resize-window", "-t", &hidden, "-x", "348", "-y", "62"]);
    let hidden_rail = real_tmux_ok(
        &raw,
        &socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            "31",
            "-t",
            &hidden,
            "-P",
            "-F",
            "#{pane_id}",
            "sleep",
            "120",
        ],
    );
    real_tmux_ok(
        &raw,
        &socket,
        &["set-option", "-p", "-t", &hidden_rail, "@organization_sidebar", "1"],
    );
    real_tmux_ok(&raw, &socket, &["set-option", "-t", &session, "@chief_sidebar_columns", "31"]);
    let before = real_tmux_ok(&raw, &socket, &["list-panes", "-t", &hidden, "-F", "#{pane_id}"]);

    let watch = ViewportWatchRunner {
        inner: raw,
        rail: hidden_rail.clone(),
        frames: std::sync::Mutex::new(Vec::new()),
    };
    let exec = RealHostExecutor::new(
        TmuxHost::new(watch, RecordingWaiter::default()),
        ProcReader::default(),
    );
    let repaired = repair_session_rails_with(
        &exec,
        &socket,
        &session,
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    );
    let watcher = exec.tmux_host().runner();
    let frames = watcher.frames.lock().expect("viewport frame lock").clone();
    let after =
        real_tmux_ok(&watcher.inner, &socket, &["list-panes", "-t", &hidden, "-F", "#{pane_id}"]);
    let rails = real_tmux_ok(
        &watcher.inner,
        &socket,
        &["list-panes", "-t", &hidden, "-F", "#{@organization_sidebar}"],
    );
    let remembered = real_tmux_ok(
        &watcher.inner,
        &socket,
        &["show-options", "-q", "-v", "-t", &session, "@chief_sidebar_columns"],
    );
    real_tmux_ok(&watcher.inner, &socket, &["kill-server"]);

    assert_eq!(repaired.expect("viewport repair"), 0, "no rail is minted");
    assert!(!frames.is_empty(), "the watcher sampled no publication boundary");
    assert!(
        frames.iter().all(|(_, _, rail, _)| *rail == 31),
        "the rail must stay at the effective custom width at every boundary: {frames:?}"
    );
    assert_eq!(frames.last(), Some(&(240, 56, 31, "manual".to_owned())));
    assert_eq!(after, before, "the same body and rail panes survive the repair");
    assert_eq!(rails.lines().filter(|marker| marker.trim() == "1").count(), 1);
    assert_eq!(remembered, "31", "viewport repair never writes the sidebar preference");
}

/// THE OPERATOR KEEPS THE CURSOR, PROVED AGAINST A REAL TMUX SERVER.
///
/// The argv tests above pin the frame; only tmux can answer what the frame
/// DOES. A real window that is missing its rail is repaired here, and the pane
/// left active afterwards must be the person's pane and not the rail tmux made
/// active when it split. `remain-on-exit` keeps the rail pane after its
/// stand-in program exits, so the question this test asks stays askable.
#[test]
fn real_rail_repair_leaves_the_person_pane_active_and_not_the_rail() {
    let raw = SystemTmuxRunner::default();
    let socket = Socket(format!("chiefd-rail-cursor-{}", std::process::id()));
    let session = format!("chiefd-rail-cursor-{}", std::process::id());
    real_tmux_ok(
        &raw,
        &socket,
        &["new-session", "-d", "-s", &session, "-x", "240", "-y", "56", "sleep", "120"],
    );
    real_tmux_ok(
        &raw,
        &socket,
        &["set-option", "-w", "-t", &session, "@organization_window_id", "executive"],
    );
    real_tmux_ok(&raw, &socket, &["set-option", "-t", &session, "@organization_id", "cobalt"]);
    real_tmux_ok(&raw, &socket, &["set-option", "-w", "-t", &session, "remain-on-exit", "on"]);
    let person = real_tmux_ok(&raw, &socket, &["list-panes", "-t", &session, "-F", "#{pane_id}"])
        .trim()
        .to_owned();

    let exec = RealHostExecutor::new(
        TmuxHost::new(raw, RecordingWaiter::default()),
        ProcReader::default(),
    );
    let repaired = repair_session_rails_with(
        &exec,
        &socket,
        &session,
        "/tmp",
        std::path::Path::new("/bin/false"),
    );
    let panes = real_tmux_ok(
        exec.tmux_host().runner(),
        &socket,
        &[
            "list-panes",
            "-t",
            &session,
            "-F",
            "#{pane_id}\t#{pane_active}\t#{@organization_sidebar}",
        ],
    );
    real_tmux_ok(exec.tmux_host().runner(), &socket, &["kill-server"]);

    assert_eq!(repaired.expect("the rail is repaired"), 1, "one rail was missing: {panes}");
    let rows: Vec<Vec<&str>> = panes.lines().map(|line| line.split('\t').collect()).collect();
    assert_eq!(rows.len(), 2, "the person pane and its new rail: {panes}");
    let active: Vec<&str> = rows.iter().filter(|row| row[1] == "1").map(|row| row[0]).collect();
    assert_eq!(active, vec![person.as_str()], "the operator is left in their own pane: {panes}");
    let rail = rows.iter().find(|row| row[0] != person).expect("the minted rail");
    assert_eq!(rail[2], "1", "and the rail it did not move to is tagged: {panes}");
}

/// A browser-like attached client must not publish tmux's proportional split.
///
/// The old test accepted that bad frame and only checked that a later repair
/// put the rail back. The live 240 -> 360 resize proved why that is not enough:
/// the operator saw 53 then 73 columns for 276 ms. Once Chief owns a managed
/// window, the first server boundary after `client-resized` must still show the
/// effective rail width; the callback may change the outer geometry later, but
/// tmux may not redraw its own intermediate layout.
#[test]
fn real_active_client_growth_never_publishes_a_proportional_rail_width() {
    let raw = SystemTmuxRunner::default();
    let socket = Socket(format!("chiefd-active-viewport-{}", std::process::id()));
    let session = format!("chiefd-active-viewport-{}", std::process::id());
    real_tmux_ok(
        &raw,
        &socket,
        &["new-session", "-d", "-s", &session, "-x", "240", "-y", "56", "sleep", "120"],
    );
    real_tmux_ok(
        &raw,
        &socket,
        &["set-option", "-w", "-t", &session, "@organization_window_id", "executive"],
    );
    real_tmux_ok(&raw, &socket, &["set-option", "-t", &session, "@organization_id", "cobalt"]);
    let rail = real_tmux_ok(
        &raw,
        &socket,
        &[
            "split-window",
            "-h",
            "-b",
            "-l",
            "26",
            "-t",
            &session,
            "-P",
            "-F",
            "#{pane_id}",
            "sleep",
            "120",
        ],
    );
    real_tmux_ok(&raw, &socket, &["set-option", "-p", "-t", &rail, "@organization_sidebar", "1"]);
    real_tmux_ok(&raw, &socket, &["set-option", "-t", &session, "@chief_sidebar_columns", "26"]);

    let exec = RealHostExecutor::new(
        TmuxHost::new(raw, RecordingWaiter::default()),
        ProcReader::default(),
    );
    repair_session_rails_with(
        &exec,
        &socket,
        &session,
        "/data/cobalt",
        std::path::Path::new("/opt/chief/bin/chief"),
    )
    .expect("the session is under Chief management before the client resize");

    let mut browser =
        ControlClient::connect("tmux", &socket, &session).expect("browser control client");
    let resized = browser
        .run(&Line { text: "refresh-client -C 348x59".to_owned(), blocks: 1 })
        .expect("browser viewport resize")
        .into_out();
    assert_eq!(resized.status, 0, "client resize: {}", resized.stderr);
    let first_boundary = real_tmux_ok(
        exec.tmux_host().runner(),
        &socket,
        &["display-message", "-p", "-t", &rail, "-F", "#{window_width}\t#{pane_width}"],
    );
    resize_session_viewport_with(
        &exec,
        &socket,
        &session,
        crate::window_geometry::Geometry { columns: 348, rows: 59 },
    )
    .expect("ordered callback publishes the final larger viewport");
    let grown = real_tmux_ok(
        exec.tmux_host().runner(),
        &socket,
        &["display-message", "-p", "-t", &rail, "-F", "#{window_width}\t#{pane_width}"],
    );
    let mut second_client = ControlClient::connect("tmux", &socket, &session)
        .expect("second ignore-size control client");
    let second_resize = second_client
        .run(&Line { text: "refresh-client -C 420x90".to_owned(), blocks: 1 })
        .expect("second client resize")
        .into_out();
    assert_eq!(second_resize.status, 0, "second client resize: {}", second_resize.stderr);
    let after_second_client = real_tmux_ok(
        exec.tmux_host().runner(),
        &socket,
        &["display-message", "-p", "-t", &rail, "-F", "#{window_width}\t#{pane_width}"],
    );
    resize_session_viewport_with(
        &exec,
        &socket,
        &session,
        crate::window_geometry::Geometry { columns: 240, rows: 56 },
    )
    .expect("ordered callback publishes the final smaller viewport");
    let shrunk = real_tmux_ok(
        exec.tmux_host().runner(),
        &socket,
        &["display-message", "-p", "-t", &rail, "-F", "#{window_width}\t#{pane_width}"],
    );
    let remembered = real_tmux_ok(
        exec.tmux_host().runner(),
        &socket,
        &["show-options", "-q", "-v", "-t", &session, "@chief_sidebar_columns"],
    );
    drop(browser);
    real_tmux_ok(exec.tmux_host().runner(), &socket, &["kill-server"]);

    assert!(
        first_boundary.ends_with("\t26"),
        "the first client-resize publication must keep the 26-column rail, not expose tmux's \
         proportional split: {first_boundary:?}"
    );
    assert_eq!(grown, "348\t26");
    assert_eq!(after_second_client, "348\t26", "ignore-size clients do not own geometry");
    assert_eq!(shrunk, "240\t26");
    assert_eq!(remembered, "26", "viewport repair never writes the human preference");
    drop(second_client);
}

fn observed_window(tmux_id: &str, logical: &str) -> plan::ObservedWindow {
    plan::ObservedWindow {
        tmux_id: tmux_id.into(),
        organization_id: "cobalt".into(),
        logical_id: logical.into(),
        protected_ui: false,
        sleeping_notice: false,
    }
}

fn observed_pane(
    tmux_id: &str,
    window_tmux: &str,
    person: &str,
    launch_hash: &str,
) -> plan::ObservedPane {
    plan::ObservedPane {
        tmux_id: tmux_id.into(),
        tmux_window_id: window_tmux.into(),
        organization_id: "cobalt".into(),
        logical_window_id: "eng".into(),
        person_id: person.into(),
        launch_hash: launch_hash.into(),
        start_command: String::new(),
    }
}

/// A pane_identity reply: `pid\tsession\torg\tperson\tlaunch_hash`.
///
/// The fifth field is the launch hash, because that is what the actuator
/// diffs on.
fn identity_reply(person: &str, launch_hash: &str) -> ScriptedReply {
    identity_reply_with_pid(4242, person, launch_hash)
}

fn identity_reply_with_pid(pid: i32, person: &str, launch_hash: &str) -> ScriptedReply {
    ScriptedReply::ok(&format!("{pid}\tcobalt-session\tcobalt\t{person}\t{launch_hash}"))
}

fn plan_of(steps: Vec<plan::Step>) -> plan::ConvergePlan {
    plan::ConvergePlan {
        steps,
        predicted_respawn_persons: Vec::new(),
        predicted_kill_panes: Vec::new(),
        warnings: Vec::new(),
        ..Default::default()
    }
}

/// P1 positive-control Rust half. The dedicated CI probe job supplies the
/// explicit artifact/test/correlation environment. This uses the production
/// host executor to create the real isolated tmux session that the report is
/// about; equal metadata alone is not evidence.
///
/// # The socket is VIRGIN, and that is a change
///
/// It was `tmux-p1-control-socket`, a fixed name, because the name was a
/// cross-language contract: a TypeScript half mutated that exact server. That
/// half is gone — E0-S2 removed the `tmux-single-writer-p1` evidence lane on
/// 2026-08-04, and what carries the name today
/// (`tests/tmux-single-writer-p1-probe-verifier.test.ts`) reads a JSONL
/// artifact and touches no tmux at all. So the contract the name served does
/// not exist, and what the name still bought was a collision: two chief-cli
/// test processes running at once each opened by killing the other's server,
/// which fails 7 runs in 8 when measured. CI runs `--lib` and `--bin chief`
/// side by side in one job today and is kept green here only by `actuate` not
/// being in the binary.
///
/// A per-call socket nobody has ever served closes that by construction, and
/// takes the whole settle apparatus with it: the defensive `kill-server`, the
/// retrying `settle_p1_server` mint that existed to survive racing it, and the
/// `kill-session` that cleaned up after it. A socket with no teardown to race
/// needs none of them. The name is built the way this module's other live
/// sockets are — pid plus a fresh uuid — because `tmux::test_support` lives in
/// the binary and `actuate` is library-side.
#[test]
fn p1_forced_dual_writer_control_creates_the_physical_ts_target() {
    let socket = Socket(format!(
        "chief-p1-control-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let exec = RealHostExecutor::production();
    let temp = tempfile::tempdir().expect("P1 fixture tempdir");
    let pi = temp.path().join("probe-pi");
    crate::files::publish_atomically(&pi, "#!/bin/sh\nsleep 30\n", 0o755)
        .expect("P1 fixture probe pi");
    let mut desired = desired_one_window();
    desired.organization = "tmux-p1-control-org".into();
    desired.session = "tmux-p1-control-session".into();
    let report = apply_plan(
        &exec,
        &socket,
        &desired,
        &empty_observed(false),
        &BTreeMap::from([(
            "vera".into(),
            LaunchSpec {
                pi_binary: pi,
                pi_home: temp.path().to_path_buf(),
                workspace: temp.path().to_path_buf(),
                ..launch("vera")
            },
        )]),
        &plan_of(vec![plan::Step::CreateSession {
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        }]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let status = std::process::Command::new("tmux")
        .args(["-L", &socket.0, "has-session", "-t", &desired.session])
        .status()
        .expect("tmux must be executable for the P1 real-seam control");
    // This server is nobody else's evidence now, so it is taken down rather
    // than left standing — one leaked socket per run is how /tmp fills with
    // dead entries and a REAL leak stops being visible.
    let _ = std::process::Command::new("tmux").args(["-L", &socket.0, "kill-server"]).status();
    assert!(status.success(), "the interpreter must create the physical target TS will mutate");
}

fn verbs(calls: &[Vec<String>]) -> Vec<String> {
    calls.iter().map(|c| c.first().cloned().unwrap_or_default()).collect()
}

/// The word each of the three fresh-session messages is recognised by here.
#[derive(Clone, Copy)]
enum Told {
    /// The founding boot: "your company was created moments ago".
    IntroduceYourself,
    /// No assigned work: up, available, and doing nothing.
    Idle,
    /// Work is waiting: the sentence this product has always sent.
    GetToWork,
}

/// THE WIRING of the boot-standing discriminator, at the seam that derives it.
///
/// `spawn_cmd`'s own tests pin the three messages and the rule that selects
/// them; what only this seam can prove is that the actuator feeds that rule the
/// COMPANY's roster and the MAILBOX FACT chiefd published, and not something
/// else. All three people here are equally session-less, which is the whole
/// point — "this person has never run" is true of every one of them and tells
/// you nothing about whether anybody has asked them for anything.
///
/// The operator saw both failures. The one-person case is a company created
/// seconds ago whose CEO "started creating departments and stuff" instead of
/// waiting. The staffed-but-idle case is a company of five sleeping people
/// nobody had asked for anything: two `Wake Up` clicks produced an Engineering
/// department, a hire, a recall and six messages about "critical chiefd
/// blockers" in two minutes. The third case is the guard against
/// over-correcting — mail waiting IS assigned work, and that person still gets
/// to work the moment their pane comes up.
#[test]
fn a_boot_is_told_to_work_only_when_something_is_actually_waiting() {
    for (roster, pending_mail, told) in [
        (vec!["vera"], false, Told::IntroduceYourself),
        (vec!["vera", "theo"], false, Told::Idle),
        (vec!["vera", "theo"], true, Told::GetToWork),
    ] {
        let exec =
            executor(ScriptedTmux::always(ScriptedReply::ok("%1\t@1\t4242\tcobalt-session")));
        let mut desired = desired_one_window();
        desired.known_person_ids = roster.iter().map(|p| (*p).to_owned()).collect();
        let mut launch_specs = launches(&roster);
        for spec in launch_specs.values_mut() {
            spec.pending_mail = pending_mail;
        }
        let report = apply_plan(
            &exec,
            &socket(),
            &desired,
            &empty_observed(false),
            &launch_specs,
            &plan_of(vec![plan::Step::CreateSession {
                first: plan::SpawnSpec {
                    person_id: "vera".into(),
                    launch_hash: "hash-2".to_owned(),
                },
            }]),
        );
        assert!(report.succeeded(), "failure: {:?}", report.failure);
        let calls = exec.tmux_host().runner().calls();
        let argv = &calls[0];
        let message = argv
            .iter()
            .find(|word| word.starts_with("You are vera (vera)"))
            .unwrap_or_else(|| panic!("the fresh-session message is in the pane argv: {argv:?}"));
        match told {
            Told::IntroduceYourself => {
                assert!(message.contains("created moments ago"), "{message}");
                assert!(message.contains("Create no department, hire nobody"), "{message}");
            }
            Told::Idle => {
                assert!(message.contains("nothing is assigned to you"), "{message}");
                assert!(message.contains("Create no department, hire nobody"), "{message}");
                assert!(!message.contains("created moments ago"), "{message}");
                assert!(!message.contains("continue the next real piece of work"), "{message}");
                // The ban comes off exactly here: an acknowledgement is the
                // correct turn, and forbidding it is what made hunting for
                // work the cheapest way to comply.
                assert!(!message.contains("acknowledgement-only"), "{message}");
            }
            Told::GetToWork => {
                assert!(message.contains("continue the next real piece of work"), "{message}");
                assert!(message.contains("acknowledgement-only"), "{message}");
                assert!(!message.contains("created moments ago"), "{message}");
                assert!(!message.contains("nothing is assigned to you"), "{message}");
            }
        }
    }
}

#[test]
fn create_session_mints_and_tags_everything_in_one_tmux_invocation() {
    // §2.0(2) ONE SHOT (F12, architecture-audit Step 2): the session, its
    // first window, its first pane AND every identity tag ride a SINGLE tmux
    // client message (one argv, `;`-separated commands), so a crash can leave
    // either nothing or a fully identified object — never a torn one.
    let scripted = ScriptedTmux::always(ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = empty_observed(false);
    let steps = vec![plan::Step::CreateSession {
        first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);

    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 1, "the whole creation is ONE tmux invocation: {calls:?}");
    let argv = &calls[0];
    assert_eq!(argv[0], "start-server");
    let new_session =
        argv.iter().position(|arg| arg == "new-session").expect("new-session command");
    for expected in [
        "extended-keys",
        // The sidebar is a mouse surface and the status bar is deleted: both
        // are server-wide options that must be in place BEFORE the first pane
        // is spawned, or the first thing the operator sees is a status bar
        // that then disappears.
        "mouse",
        "status",
        "escape-time",
        "set-clipboard",
        "terminal-features",
        "copy-mode",
        "copy-mode-vi",
    ] {
        let configured =
            argv.iter().position(|arg| arg.contains(expected)).expect("input configuration");
        assert!(
            configured < new_session,
            "{expected} must be configured before new-session: {argv:?}"
        );
    }
    assert_eq!(
        argv.iter()
            .filter(|arg| arg.as_str() == "set-option -as terminal-features ,xterm*:RGB")
            .count(),
        1,
        "the RGB rule is conditionally appended once: {argv:?}",
    );
    assert!(
        argv.iter()
            .any(|arg| arg == "tmux show-options -s -v terminal-features | grep -Fxq 'xterm*:RGB'"),
        "the terminal-features append is guarded by the live server value: {argv:?}",
    );
    // The new-session argv carries the pane launch after `--`.
    assert!(argv.iter().any(|a| a == "/opt/pi/bin/pi"), "argv: {argv:?}");
    assert!(argv.iter().any(|a| a == "engineering"));
    // Every identity tag follows inside the same message: session, window and
    // pane ownership follow the input commands and `new-session` in the same
    // ordered command queue.
    // Twelve input-configuration commands (extended-keys, mouse, status, the
    // prefix-free unzoom binding, escape-time, set-clipboard, the guarded
    // RGB, extkeys and sync terminal-feature appends, the two copy-mode
    // bindings and the explicit rail-border release binding), `new-session`,
    // and seven identity commands.
    assert_eq!(argv.iter().filter(|a| a.as_str() == ";").count(), 20, "argv: {argv:?}");
    // THE SYNC FEATURE, because a window switch that repaints cell by cell is
    // the flicker the operator reported three times. Guarded exactly like the
    // extkeys rule so a server shared by several companies collects it once.
    assert!(
        argv.iter().any(|arg| arg == "set-option -as terminal-features ,*:sync"),
        "the sync terminal feature is declared: {argv:?}",
    );
    assert!(
        argv.iter()
            .any(|arg| arg == "tmux show-options -s -v terminal-features | grep -Fq '*:sync'"),
        "the sync append is guarded by the live server value: {argv:?}",
    );
    let sync = argv
        .iter()
        .position(|arg| arg == "set-option -as terminal-features ,*:sync")
        .expect("sync rule");
    assert!(sync < new_session, "sync is declared before the first pane: {argv:?}");
    assert!(
        argv.windows(4)
            .any(|window| { window == ["bind-key", "-T", "root", "MouseDragEnd1Border"] }),
        "the release binding is installed before the first pane: {argv:?}"
    );
    assert!(
        argv.iter().any(|arg| arg.contains("@organization_sidebar"))
            && argv.iter().any(|arg| arg.contains("@chief_viewport_width_command")),
        "the binding validates the rail tag before it calls the exact width authority: {argv:?}"
    );
    for expected in [
        "@organization_id",
        "cobalt",
        "@organization_window_id",
        "eng",
        "@organization_person_id",
        "vera",
        "@organization_launch_hash",
        "hash-2",
    ] {
        assert!(argv.iter().any(|a| a == expected), "missing {expected} in argv: {argv:?}");
    }
    // Session ownership is the FIRST follow-on command. Observation never
    // destroys an unowned session; it fails closed on one empty read.
    let first_tag = argv
        .iter()
        .enumerate()
        .skip(new_session + 1)
        .find_map(|(index, arg)| (arg == ";").then_some(index))
        .expect("an identity follow-on command");
    assert_eq!(argv[first_tag + 1], "set-option");
    assert_eq!(argv[first_tag + 2], "-t");
    assert_eq!(argv[first_tag + 3], "cobalt-session");
    assert_eq!(argv[first_tag + 4], "@organization_id");
    assert_eq!(argv[first_tag + 5], "cobalt");
    assert!(
        argv.iter().all(|word| word != "@organization_minting"),
        "session creation has no obsolete mint marker: {argv:?}"
    );
    // The window/pane tags address the fresh session's current window/pane
    // (`cobalt-session:`) — the minted ids are unknowable at argv-build time.
    assert!(argv.iter().any(|a| a == "cobalt-session:"), "argv: {argv:?}");
}

#[test]
fn a_person_absent_from_the_iterated_launch_roster_names_the_roster_not_the_gate() {
    // #52's shape: the M1 planner spawned a pane for a person the launch
    // catalog never even iterated (a people_order/roster mismatch), not one
    // it iterated and the resource gate refused. Without
    // `iterated_launch_roster`, both looked identical ("no launch spec for
    // person 'x'") and that ambiguity cost hours chasing an uncalled
    // function. With it supplied, the roster-absent case names itself.
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::always(ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = empty_observed(false);
    let steps = vec![plan::Step::CreateSession {
        first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];
    let roster: BTreeSet<String> = BTreeSet::new(); // "vera" iterated by nobody

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            catalog: &BTreeMap::new(), // launch catalog empty either way
            diagnostics: super::LaunchRosterDiagnostics {
                iterated_launch_roster: Some(&roster),
                refusal_reasons: None,
            },
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );
    let failure = report.failure.expect("must fail: no launch spec available for vera");
    let message = failure.to_string();
    assert!(
        message.contains("not in the launch roster") && message.contains("0 people iterated"),
        "expected the roster-absent diagnostic naming the count, got: {message}",
    );
}

#[test]
fn a_person_in_the_iterated_roster_but_refused_is_skipped_and_named_not_failed() {
    // The other half of the same branch: present in the roster, absent from
    // `launch` (the gate refused them). It must not regress to the
    // roster-absent wording just because a roster is now supplied -- and it is
    // no longer a failure at all. A person the gate declined is an expected
    // condition; it costs them their step and nothing else.
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::always(ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = empty_observed(false);
    let steps = vec![plan::Step::CreateSession {
        first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];
    let roster: BTreeSet<String> = BTreeSet::from(["vera".to_owned()]); // iterated...

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            // ...but the resource gate refused them (empty launch catalog)
            catalog: &BTreeMap::new(),
            diagnostics: super::LaunchRosterDiagnostics {
                iterated_launch_roster: Some(&roster),
                refusal_reasons: None,
            },
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );
    assert!(
        report.failure.is_none(),
        "a refused person is not a broken plan: {:?}",
        report.failure
    );
    let reason = report.refused.get("vera").expect("vera is named as refused");
    assert!(
        !reason.contains("roster"),
        "the roster-absent diagnostic is a different, still-fatal case: {reason}",
    );
    assert!(
        reason.contains("no launch spec and no reason"),
        "a refusal chiefd published no reason for still says so: {reason}",
    );
}

#[test]
fn a_refused_person_with_a_precomputed_reason_is_skipped_naming_the_refusing_check() {
    // Third case: present in the roster, refused, AND the caller (cycle.rs,
    // via explain_launch_refusal) has already re-derived WHY. "no launch
    // spec" collapsed a missing directory and a failed credential stage into
    // one interchangeable message; naming the check is what a human actually
    // needs to act on it.
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::always(ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = empty_observed(false);
    let steps = vec![plan::Step::CreateSession {
        first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];
    let roster: BTreeSet<String> = BTreeSet::from(["vera".to_owned()]);
    let reasons = BTreeMap::from([(
        "vera".to_owned(),
        "native provider credential staging refused (provider=openrouter)".to_owned(),
    )]);

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            catalog: &BTreeMap::new(),
            diagnostics: super::LaunchRosterDiagnostics {
                iterated_launch_roster: Some(&roster),
                refusal_reasons: Some(&reasons),
            },
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );
    assert!(report.failure.is_none(), "a refused person is not a broken plan");
    assert_eq!(
        report.refused.get("vera").map(String::as_str),
        Some("native provider credential staging refused (provider=openrouter)"),
        "the precomputed reason is surfaced verbatim, on the skip rather than on a failure",
    );
}

#[test]
fn real_tmux_configures_input_and_preserves_exact_rgb_through_an_xterm_client() {
    let runner = SystemTmuxRunner::default();
    let socket = Socket(format!("chiefd-input-order-{}", std::process::id()));
    let directory = tempfile::tempdir().expect("tempdir");
    let observation = directory.path().join("input.txt");
    let signal = format!("chiefd-input-ready-{}", std::process::id());
    let session = format!("chiefd-input-order-{}", std::process::id());

    let mut argv = vec!["start-server".to_owned()];
    super::push_server_input_configuration(&mut argv);
    super::push_tmux_command(
        &mut argv,
        [
            "new-session".to_owned(),
            "-d".to_owned(),
            "-s".to_owned(),
            session.clone(),
            "--".to_owned(),
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "printf '\\033[38;2;91;33;182m\\033[48;2;237;231;246mLIGHT\\033[0m\\n\\
                     \\033[38;2;216;180;254m\\033[48;2;46;16;101mDARK\\033[0m\\n'; \
             tmux show-options -s -v extended-keys > \"$1\"; \
             tmux wait-for -S \"$2\"; sleep 30"
                .to_owned(),
            "chiefd-input-observer".to_owned(),
            observation.display().to_string(),
            signal.clone(),
        ],
    );
    super::push_tmux_command(&mut argv, ["wait-for".to_owned(), signal]);
    let created = runner.run(&socket, &TmuxCmd { argv }).expect("run ordered queue");
    assert_eq!(created.status, 0, "tmux: {}", created.stderr);
    assert_eq!(std::fs::read_to_string(&observation).expect("pane observation").trim(), "on");

    let mut second = vec!["start-server".to_owned()];
    super::push_server_input_configuration(&mut second);
    super::push_tmux_command(&mut second, ["show-options", "-s", "-v", "terminal-features"]);
    let repeated = runner.run(&socket, &TmuxCmd { argv: second }).expect("repeat configuration");
    assert_eq!(repeated.status, 0, "tmux: {}", repeated.stderr);
    assert_eq!(
        repeated.stdout.lines().filter(|line| *line == "xterm*:RGB").count(),
        1,
        "the conditional append remains idempotent: {}",
        repeated.stdout,
    );

    let typescript = directory.path().join("xterm-rgb.typescript");
    let mut client = std::process::Command::new("script")
        .args([
            "-q",
            "-c",
            &format!("tmux -L {} attach-session -t {session}", socket.0),
            typescript.to_str().expect("typescript path"),
        ])
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("ordinary browser-shaped tmux client");
    let _client_input = client.stdin.take().expect("keep the ordinary client open");
    let mut client_name = String::new();
    for _ in 0..40 {
        client_name = runner
            .run(
                &socket,
                &TmuxCmd {
                    argv: vec!["list-clients".into(), "-F".into(), "#{client_name}".into()],
                },
            )
            .expect("list ordinary client")
            .stdout
            .trim()
            .to_owned();
        if !client_name.is_empty() {
            break;
        }
        // os-liveness: this test waits for the real external tmux client to
        // appear. No injected product clock can advance that process.
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(!client_name.is_empty(), "ordinary client attached");
    let features = runner
        .run(
            &socket,
            &TmuxCmd {
                argv: vec![
                    "display-message".into(),
                    "-p".into(),
                    "-c".into(),
                    client_name.clone(),
                    "#{client_termfeatures}".into(),
                ],
            },
        )
        .expect("read the exact client's negotiated features");
    assert!(features.stdout.split(',').any(|feature| feature.trim() == "RGB"), "{features:?}");
    // os-liveness: let the real external terminal client consume the pane's
    // first draw before it is detached. No injected product clock owns it.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_millis(50));
    let detached = runner
        .run(&socket, &TmuxCmd { argv: vec!["detach-client".into(), "-t".into(), client_name] })
        .expect("detach ordinary client");
    assert_eq!(detached.status, 0, "tmux: {}", detached.stderr);
    let status = client.wait().expect("ordinary client exits after detach");
    assert!(status.success(), "script client: {status}");
    let bytes = std::fs::read(&typescript).expect("captured terminal stream");
    for exact in [
        b"\x1b[38;2;91;33;182m".as_slice(),
        b"\x1b[48;2;237;231;246m".as_slice(),
        b"\x1b[38;2;216;180;254m".as_slice(),
        b"\x1b[48;2;46;16;101m".as_slice(),
    ] {
        assert!(
            bytes.windows(exact.len()).any(|window| window == exact),
            "exact RGB was lost: {bytes:?}"
        );
    }
    for quantized in [
        b"\x1b[38;5;55m".as_slice(),
        b"\x1b[48;5;255m".as_slice(),
        b"\x1b[38;5;183m".as_slice(),
        b"\x1b[48;5;17m".as_slice(),
    ] {
        assert!(
            !bytes.windows(quantized.len()).any(|window| window == quantized),
            "tmux quantized an exact RGB color: {bytes:?}"
        );
    }
    let keys = runner
        .run(&socket, &TmuxCmd { argv: vec!["list-keys".into(), "-T".into(), "root".into()] })
        .expect("list root bindings");
    assert!(
        keys.stdout.contains("MouseDrag1Border") && keys.stdout.contains("resize-pane -M"),
        "the default live drag remains intact: {}",
        keys.stdout
    );
    assert!(
        keys.stdout.contains("MouseDragEnd1Border")
            && keys.stdout.contains("@organization_sidebar")
            && keys.stdout.contains("@chief_viewport_width_command"),
        "only the tagged rail-border release records a width: {}",
        keys.stdout
    );

    let _ = runner.run(&socket, &TmuxCmd { argv: vec!["kill-server".to_owned()] });
}

#[derive(Clone)]
struct BoundarySamplingRunner {
    inner: SystemTmuxRunner,
    session: String,
    samples: Arc<Mutex<Vec<String>>>,
}

impl TmuxRunner for BoundarySamplingRunner {
    fn run(
        &self,
        socket: &Socket,
        cmd: &TmuxCmd,
    ) -> Result<crate::actuate::host::TmuxOut, crate::actuate::host::HostErr> {
        let result = self.inner.run(socket, cmd)?;
        let sample = std::process::Command::new("tmux")
            .args([
                "-L",
                &socket.0,
                "list-panes",
                "-t",
                &self.session,
                "-F",
                "#{pane_id}\t#{pane_width}\t#{pane_height}\t#{@organization_sidebar}\t#{@chief_asleep_for}\t#{@organization_person_id}",
            ])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        self.samples.lock().expect("sample lock").push(sample);
        Ok(result)
    }
}

/// A deleted department is a one-way tmux publication. Its final removal is
/// one `kill-window` server command: every sampled command boundary sees the
/// complete old rail+body or no old window at all. A later settled pass sends
/// no command and therefore cannot rebuild the deleted body.
#[test]
fn real_tmux_removes_a_deleted_department_once_without_a_rail_only_frame_or_respawn() {
    let socket = Socket(format!("chiefd-deleted-department-{}", std::process::id()));
    let session = format!("cobalt-deleted-department-{}", std::process::id());
    let setup = SystemTmuxRunner::default();
    let root = real_tmux_ok(
        &setup,
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            &session,
            "-n",
            "Engineering",
            "-x",
            "160",
            "-y",
            "30",
            "-P",
            "-F",
            "#{window_id}\t#{pane_id}",
            "--",
            "sleep",
            "30",
        ],
    );
    let (root_window, root_pane) = root.split_once('\t').expect("root ids");
    let removed = real_tmux_ok(
        &setup,
        &socket,
        &[
            "new-window",
            "-d",
            "-t",
            &session,
            "-n",
            "Go-To-Market",
            "-P",
            "-F",
            "#{window_id}\t#{pane_id}",
            "--",
            "sleep",
            "30",
        ],
    );
    let (removed_window, removed_body) = removed.split_once('\t').expect("removed ids");
    let removed_rail = real_tmux_ok(
        &setup,
        &socket,
        &[
            "split-window",
            "-h",
            "-l",
            "26",
            "-t",
            removed_window,
            "-P",
            "-F",
            "#{pane_id}",
            "--",
            "sleep",
            "30",
        ],
    );
    real_tmux_ok(
        &setup,
        &socket,
        &[
            "set-option",
            "-t",
            &session,
            "@organization_id",
            "cobalt",
            ";",
            "set-option",
            "-w",
            "-t",
            root_window,
            "@organization_id",
            "cobalt",
            ";",
            "set-option",
            "-w",
            "-t",
            root_window,
            "@organization_window_id",
            "eng",
            ";",
            "set-option",
            "-p",
            "-t",
            root_pane,
            "@organization_person_id",
            "vera",
            ";",
            "set-option",
            "-w",
            "-t",
            removed_window,
            "@organization_id",
            "cobalt",
            ";",
            "set-option",
            "-w",
            "-t",
            removed_window,
            "@organization_window_id",
            "go-to-market",
            ";",
            "set-option",
            "-p",
            "-t",
            removed_body,
            "@organization_person_id",
            "mara",
            ";",
            "set-option",
            "-p",
            "-t",
            &removed_rail,
            "@organization_sidebar",
            "1",
            ";",
            "select-window",
            "-t",
            root_window,
        ],
    );

    let samples = Arc::new(Mutex::new(Vec::new()));
    let exec = RealHostExecutor::new(
        TmuxHost::new(
            BoundarySamplingRunner {
                inner: SystemTmuxRunner::default(),
                session: session.clone(),
                samples: Arc::clone(&samples),
            },
            RecordingWaiter::default(),
        ),
        ProcReader::default(),
    );
    let mut desired = desired_one_window();
    desired.session = session.clone();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window(removed_window, "go-to-market")],
        panes: Vec::new(),
    };
    let report = apply_plan(
        &exec,
        &socket,
        &desired,
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::KillWindow {
            w: plan::WindowRef::Observed(removed_window.into()),
        }]),
    );
    assert!(report.succeeded(), "deleted-window removal: {:?}", report.failure);

    let boundaries = samples.lock().expect("sample lock").clone();
    assert!(!boundaries.is_empty());
    for (boundary, sample) in boundaries.iter().enumerate() {
        let has_body = sample.lines().any(|line| line.starts_with(removed_body));
        let has_rail = sample.lines().any(|line| line.starts_with(&removed_rail));
        assert_eq!(
            has_body, has_rail,
            "boundary {boundary} exposed the deleted rail without its body: {sample:?}"
        );
    }
    let final_sample = boundaries.last().expect("final boundary");
    assert!(!final_sample.contains(removed_body));
    assert!(!final_sample.contains(&removed_rail));
    assert!(final_sample.contains(root_pane), "the surviving company is unchanged");

    let before_settled = samples.lock().expect("sample lock").len();
    let settled = apply_plan(
        &exec,
        &socket,
        &desired,
        &plan::ObservedTopology {
            session_exists: true,
            session_organization: "cobalt".into(),
            windows: Vec::new(),
            panes: Vec::new(),
        },
        &launches(&["vera"]),
        &plan_of(Vec::new()),
    );
    assert!(settled.succeeded());
    assert_eq!(
        samples.lock().expect("sample lock").len(),
        before_settled,
        "the settled pass cannot remint the deleted window"
    );

    let _ = setup.run(&socket, &TmuxCmd { argv: vec!["kill-server".into()] });
}

#[test]
fn real_tmux_border_release_is_tagged_readable_and_session_local() {
    let runner = SystemTmuxRunner::default();
    let socket = Socket(format!("chiefd-human-width-{}", std::process::id()));
    let run = |argv: &[&str]| {
        runner
            .run(&socket, &TmuxCmd { argv: argv.iter().map(|arg| (*arg).to_owned()).collect() })
            .expect("tmux command")
    };
    let body_a = run(&[
        "new-session",
        "-d",
        "-s",
        "width-a",
        "-x",
        "120",
        "-y",
        "30",
        "-P",
        "-F",
        "#{pane_id}",
        "sleep",
        "30",
    ])
    .stdout
    .trim()
    .to_owned();
    let rail_a = run(&[
        "split-window",
        "-d",
        "-h",
        "-b",
        "-l",
        "37",
        "-t",
        "width-a",
        "-P",
        "-F",
        "#{pane_id}",
        "sleep",
        "30",
    ])
    .stdout
    .trim()
    .to_owned();
    let _ = run(&["set-option", "-p", "-t", &rail_a, "@organization_sidebar", "1"]);
    let _ = run(&["new-session", "-d", "-s", "width-b", "-x", "120", "-y", "30", "sleep", "30"]);
    let rail_b = run(&[
        "split-window",
        "-d",
        "-h",
        "-b",
        "-l",
        "41",
        "-t",
        "width-b",
        "-P",
        "-F",
        "#{pane_id}",
        "sleep",
        "30",
    ])
    .stdout
    .trim()
    .to_owned();
    let _ = run(&["set-option", "-p", "-t", &rail_b, "@organization_sidebar", "1"]);

    let release = |pane: &str| {
        let command = format!(
            "run-shell -t '{pane}' \"tmux set-option -t '#{{session_name}}' @chief_sidebar_columns '#{{pane_width}}'\""
        );
        run(&[
            "if-shell",
            "-F",
            "-t",
            pane,
            "#{&&:#{==:#{@organization_sidebar},1},#{e|>=:#{pane_width},12}}",
            &command,
        ])
    };
    assert_eq!(release(&rail_a).status, 0);
    assert_eq!(
        run(&["show-options", "-q", "-v", "-t", "width-a", "@chief_sidebar_columns"]).stdout.trim(),
        "37"
    );

    assert_eq!(release(&body_a).status, 0, "a body release is a clean no-op");
    assert_eq!(
        run(&["show-options", "-q", "-v", "-t", "width-a", "@chief_sidebar_columns"]).stdout.trim(),
        "37"
    );

    assert_eq!(release(&rail_b).status, 0);
    assert_eq!(
        run(&["show-options", "-q", "-v", "-t", "width-b", "@chief_sidebar_columns"]).stdout.trim(),
        "41"
    );
    assert_eq!(
        run(&["show-options", "-q", "-v", "-t", "width-a", "@chief_sidebar_columns"]).stdout.trim(),
        "37"
    );

    let _ = run(&["resize-pane", "-x", "4", "-t", &rail_b]);
    assert_eq!(release(&rail_b).status, 0, "an unreadable rail release is a clean no-op");
    assert_eq!(
        run(&["show-options", "-q", "-v", "-t", "width-b", "@chief_sidebar_columns"]).stdout.trim(),
        "41"
    );
    let _ = run(&["kill-server"]);
}

#[test]
fn split_pane_adds_a_pane_to_an_existing_window() {
    // EXPLICIT, not `always`. This fixture used to answer every call with the
    // same minted-pane row, which was harmless while the split was the first
    // thing `split_pane` did — but the probes that decide WHERE to split then
    // read that row as a pane and split beside a pane id that does not exist.
    // A scripted reply per question is the honest shape.
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""),                         // precondition: no pane for vera
        ScriptedReply::ok(""),                         // no person pane to take room from
        ScriptedReply::ok("%7\t4242\tcobalt-session"), // the split itself
        ScriptedReply::ok(""),                         // mark-minting
        ScriptedReply::ok(""),                         // tags
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // clear-minting
        ScriptedReply::ok(""), // and anything the ownership mark issues
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@3".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    // The apply-time creation precondition lists panes first; the split itself
    // must still target the existing window.
    let split = calls.iter().find(|c| c[0] == "split-window").expect("a split-window call");
    assert!(split.contains(&"@3".to_string()));
    assert_eq!(
        calls[0][0], "list-panes",
        "the creation precondition re-reads the live topology first"
    );
    assert!(calls.iter().any(|c| c[0] == "set-option" && c.contains(&"%7".to_string())));
}

#[test]
fn split_pane_retiles_and_retries_when_the_window_is_out_of_space() {
    // #522: a naive `split-window` halves ONE pane each time, so a busy window
    // eventually refuses with "no space for new pane" -- at any geometry. The
    // actuator must reclaim room by re-tiling the window `tiled` and retry the
    // split ONCE, never abort the whole apply pass over a single pane's capacity.
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""), // creation precondition: no existing pane
        ScriptedReply::ok(""), // no person pane is available to take room from
        ScriptedReply::failed("no space for new pane"), // split-window attempt 1 FAILS
        ScriptedReply::ok(""), // select-layout tiled reclaims room
        ScriptedReply::ok("%7\t4242\tcobalt-session"), // split-window retry SUCCEEDS
        ScriptedReply::ok(""), // #18 P2: mark-minting on the fresh pane
        ScriptedReply::ok(""), // pane tag 1
        ScriptedReply::ok(""), // pane tag 2
        ScriptedReply::ok(""), // pane tag 3
        ScriptedReply::ok(""), // pane tag 4
        ScriptedReply::ok(""), // #18 P2: clear-minting on the fresh pane
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@3".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "the re-tile+retry must recover the pass: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    let names = verbs(&calls);
    assert_eq!(
        names[0], "list-panes",
        "the creation precondition re-reads the live topology first"
    );
    assert_eq!(
        names[1], "list-panes",
        "then which pane the split should take room from — never the rail, whose width the \
         operator chose"
    );
    assert_eq!(names[2], "split-window", "the first split is attempted");
    assert_eq!(names[3], "select-layout", "on no-space it re-tiles to reclaim room");
    assert!(calls[3].contains(&"tiled".to_string()), "the reclaim uses the `tiled` layout");
    assert!(calls[3].contains(&"@3".to_string()), "…on the crowded window");
    assert_eq!(names[4], "split-window", "then retries the split");
    assert!(
        calls.iter().any(|c| c[0] == "set-option" && c.contains(&"%7".to_string())),
        "the retried pane id is tagged, proving the pane was created"
    );
}

#[test]
fn split_pane_defers_the_pane_when_the_window_is_full_even_tiled_instead_of_aborting() {
    // #522: if the window is at capacity even AFTER re-tiling (a department larger
    // than the window's whole geometry), the over-capacity pane is DEFERRED
    // (skipped + logged) and the pass SUCCEEDS -- never a hard-abort that would
    // block all convergence. The deferred pane is re-attempted next reconcile.
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""), // creation precondition: no existing pane
        ScriptedReply::ok(""), // no person pane is available to take room from
        ScriptedReply::failed("no space for new pane"), // split attempt 1
        ScriptedReply::ok(""), // select-layout tiled
        ScriptedReply::failed("no space for new pane"), // retry STILL no space -> defer
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@3".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(
        report.succeeded(),
        "an over-capacity pane must NOT abort the pass: {:?}",
        report.failure
    );
    let calls = exec.tmux_host().runner().calls();
    let names = verbs(&calls);
    assert_eq!(
        names,
        vec!["list-panes", "list-panes", "split-window", "select-layout", "split-window"],
        "it probes for an existing pane, then for a person pane to take room from, tries, \
         re-tiles, retries once, then \
         defers -- no further tmux calls"
    );
    assert!(
        !calls.iter().any(|c| c[0] == "set-option"),
        "a deferred pane is never tagged, because it was not created"
    );
}

#[test]
fn order_windows_moves_are_always_detached_so_attached_clients_keep_their_window() {
    // A reorder that activates each moved window in sequence lands a watching
    // operator on the last one (the historical "view jumps to the newest
    // window" complaint); every move-window must pass `-d` like the TypeScript
    // side does.
    let scripted = ScriptedTmux::always(ScriptedReply::ok(""));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng"), observed_window("@4", "quant")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::OrderWindows {
        order: vec![plan::WindowRef::Observed("@3".into()), plan::WindowRef::Observed("@4".into())],
    }];

    let report = apply_plan(&exec, &socket(), &desired, &observed, &launches(&[]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    let moves: Vec<_> = calls.iter().filter(|c| c[0] == "move-window").collect();
    assert!(!moves.is_empty(), "an order step must move windows: {:?}", calls);
    for call in moves {
        assert!(
            call.contains(&"-d".to_string()),
            "every move-window must be detached (never steal the active window): {:?}",
            call
        );
    }
}

#[test]
fn kill_pane_reverifies_ownership_and_then_kills() {
    // Identity still ours and still vera, and the operator is looking at some
    // OTHER window -> the kill proceeds.
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // The watched-window chokepoint's one read: `%9` lives in `@3`, which
        // is not active, and `@3` holds a second body pane anyway.
        ScriptedReply::ok("@1\t%1\t1\t1\n@3\t%9\t0\t\n@3\t%8\t0\t"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let steps = vec![plan::Step::KillPane { pane: plan::PaneId("%9".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    let names = verbs(&calls);
    assert!(names.contains(&"display-message".to_string()), "must re-verify first");
    assert!(names.contains(&"kill-pane".to_string()), "then kill");
    // The re-verify precedes the kill.
    let dm = names.iter().position(|v| v == "display-message").unwrap();
    let kp = names.iter().position(|v| v == "kill-pane").unwrap();
    assert!(dm < kp);
}

/// **THE CHOKEPOINT, ON THE THREE STEPS THAT NEVER HAD IT.**
///
/// The operator reported "everything jumped to the Chief" twice, and the second
/// time it was still happening after the fix they were told about. The reason
/// is that operator-safety lived INSIDE `kill_window` — one of four steps that
/// can destroy or collapse a window. `kill_pane`, `join-pane` and `break-pane`
/// each took the glass silently.
///
/// The Chief is not the target and there is no CEO-specific code anywhere: tmux
/// walks last-used → previous → next when a window dies under a client, and
/// index 0 is the Chief's. Do not go hunting for intent.
#[test]
fn kill_pane_defers_when_it_would_empty_the_window_the_operator_is_watching() {
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // `%9` is the ONLY pane of `@3`, and `@3` is what the operator is on.
        // Killing it destroys the window under them.
        ScriptedReply::ok("@1\t%1\t0\t1\n@3\t%9\t1\t"),
    ]);
    let exec = executor(scripted);
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::KillPane { pane: plan::PaneId("%9".into()) }]),
    );
    // A DEFERRAL, not a failure — nothing about the world is wrong.
    assert!(report.succeeded(), "a watched window is not an error: {:?}", report.failure);
    assert!(
        !verbs(&exec.tmux_host().runner().calls()).contains(&"kill-pane".to_string()),
        "and the pane the operator is looking at survives: {:?}",
        exec.tmux_host().runner().calls()
    );
}

/// A RAIL ALONE IS FURNITURE. Leaving the operator staring at a sidebar with no
/// body beside it is the same theft as taking the window, so the last BODY pane
/// counts as well as the last pane.
#[test]
fn kill_pane_defers_when_it_would_leave_the_watched_window_holding_only_its_rail() {
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // `@3` is active and holds two panes — but one of them is the rail
        // (`@organization_sidebar` = 1), so killing `%9` leaves furniture.
        ScriptedReply::ok("@3\t%2\t1\t1\n@3\t%9\t1\t"),
    ]);
    let exec = executor(scripted);
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::KillPane { pane: plan::PaneId("%9".into()) }]),
    );
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(
        !verbs(&exec.tmux_host().runner().calls()).contains(&"kill-pane".to_string()),
        "{:?}",
        exec.tmux_host().runner().calls()
    );
}

/// AND IT DOES NOT OVER-DEFER. A watched window that keeps a body pane is not
/// being taken from anybody, so the kill proceeds — otherwise the guard would
/// stall every reap in the window the operator happens to be reading, which is
/// the starvation `kill_window`'s own comment warns against curing by
/// weakening the deferral.
#[test]
fn kill_pane_proceeds_on_a_watched_window_that_keeps_a_body_pane() {
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // `@3` is active and keeps `%8`, a second BODY pane, after `%9` goes.
        ScriptedReply::ok("@3\t%2\t1\t1\n@3\t%9\t1\t\n@3\t%8\t1\t"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::KillPane { pane: plan::PaneId("%9".into()) }]),
    );
    assert!(report.succeeded(), "{:?}", report.failure);
    assert!(
        verbs(&exec.tmux_host().runner().calls()).contains(&"kill-pane".to_string()),
        "a window that survives the kill is not the operator's glass being taken: {:?}",
        exec.tmux_host().runner().calls()
    );
}

#[test]
fn kill_pane_toctou_a_flipped_person_tag_aborts_and_never_kills() {
    // Between observe and apply the pane became someone else's. The kill must
    // NOT fire; the cycle aborts and the next pass re-observes.
    let scripted = ScriptedTmux::new([identity_reply("intruder", "hash-2")]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let steps = vec![plan::Step::KillPane { pane: plan::PaneId("%9".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(matches!(report.failure, Some(StepError::Precondition { step: "KillPane", .. })));
    let names = verbs(&exec.tmux_host().runner().calls());
    assert!(!names.contains(&"kill-pane".to_string()), "a flipped tag must never be killed");
}

#[test]
fn kill_pane_toctou_a_foreign_org_tag_aborts_and_never_kills() {
    let scripted = ScriptedTmux::new([ScriptedReply::ok("4242\tcobalt-session\trival\tvera\t2")]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let steps = vec![plan::Step::KillPane { pane: plan::PaneId("%9".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(matches!(report.failure, Some(StepError::Precondition { step: "KillPane", .. })));
    assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"kill-pane".to_string()));
}

#[test]
fn respawn_toctou_an_already_current_launch_hash_aborts_and_never_respawns() {
    // The pane already advanced to the desired launch hash (someone else
    // respawned it). Respawning again would kill a fresh process.
    let scripted = ScriptedTmux::new([identity_reply("vera", "hash-3")]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let steps = vec![plan::Step::Respawn {
        pane: plan::PaneId("%9".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-3".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(matches!(report.failure, Some(StepError::Precondition { step: "Respawn", .. })));
    assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"respawn-pane".to_string()));
}

#[test]
fn respawn_a_still_stale_launch_hash_respawns_and_retags() {
    let scripted =
        ScriptedTmux::then_always([identity_reply("vera", "hash-2")], ScriptedReply::ok(""));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let steps = vec![plan::Step::Respawn {
        pane: plan::PaneId("%9".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-3".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    let names = verbs(&calls);
    assert!(names.contains(&"respawn-pane".to_string()));
    // The NEW launch hash is tagged after respawn. Tagging the pane with the
    // old one would leave every later pass believing it is still stale, and it
    // would be replaced again, forever.
    assert!(calls.iter().any(|c| {
        c[0] == "set-option"
            && c.contains(&"@organization_launch_hash".to_string())
            && c.contains(&"hash-3".to_string())
    }));
}

/// chiefd's own sentence for the refused person in the tests below.
const GATE_REASON: &str =
    "required files 'settings.json' and 'agent.md' are missing from home '/companies/cobalt/nix'";

/// The refusal diagnostics a production pass carries: everybody iterated, one
/// of them declined with a named reason.
fn refusing_diagnostics<'a>(
    roster: &'a std::collections::BTreeSet<String>,
    reasons: &'a BTreeMap<String, String>,
) -> super::LaunchRosterDiagnostics<'a> {
    super::LaunchRosterDiagnostics {
        iterated_launch_roster: Some(roster),
        refusal_reasons: Some(reasons),
    }
}

/// ONE REFUSED PERSON COSTS THEIR OWN STEP AND NOTHING ELSE.
///
/// A missing launch spec used to be `StepError::Internal`, and the step loop
/// returns on the first error — so a person chiefd's gate declined took every
/// healthy person ordered behind them down with them. The pass reported `the
/// pass FAILED after X of Y step(s)` and nobody at Y was ever attempted, on
/// every pass, for as long as the refusal lasted.
#[test]
fn a_refused_person_is_skipped_and_the_people_behind_them_are_still_started() {
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::then_always(
        [
            ScriptedReply::ok(""), // step 0 creation precondition: no existing pane for vera
            ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"), // step 0 new-window
            ScriptedReply::ok(""), // mark-minting on the fresh window
            ScriptedReply::ok(""), // mark-minting on the fresh pane
            ScriptedReply::ok(""), // window tag 1
            ScriptedReply::ok(""), // window tag 2
            ScriptedReply::ok(""), // pane tag 1
            ScriptedReply::ok(""), // pane tag 2
            ScriptedReply::ok(""), // pane tag 3
            ScriptedReply::ok(""), // pane tag 4
            ScriptedReply::ok(""), // clear-minting on the fresh window
            ScriptedReply::ok(""), // clear-minting on the fresh pane
            ScriptedReply::ok(""), // collapsed preference: open
            ScriptedReply::ok(""), // expanded preference: default
            ScriptedReply::ok(""), // rail mint reports no pane
            ScriptedReply::ok(""), // step 1 creation precondition for the REFUSED person
            // …and step 1 stops there: no launch spec, so no split is issued.
            ScriptedReply::ok(""), // step 2 creation precondition for theo
            ScriptedReply::ok(""), // …and no person pane to split beside
            ScriptedReply::ok("%2\t4243\tcobalt-session"), // step 2 SplitPane: theo IS started
        ],
        ScriptedReply::ok(""),
    );
    let exec = executor(scripted);
    let desired = desired_two_people_one_window();
    let observed = empty_observed(true);
    let steps = vec![
        plan::Step::CreateWindowWithSpawn {
            w: plan::WindowSym("eng".into()),
            name: "engineering".into(),
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        },
        // THE REFUSED PERSON, IN THE MIDDLE OF THE PLAN.
        plan::Step::SplitPane {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            spec: plan::SpawnSpec { person_id: "nix".into(), launch_hash: "hash-9".to_owned() },
        },
        // AND A HEALTHY PERSON QUEUED BEHIND THEM.
        plan::Step::SplitPane {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            spec: plan::SpawnSpec { person_id: "theo".into(), launch_hash: "hash-3".to_owned() },
        },
    ];
    let roster: BTreeSet<String> = ["vera", "nix", "theo"].into_iter().map(str::to_owned).collect();
    let reasons = BTreeMap::from([("nix".to_owned(), GATE_REASON.to_owned())]);

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            catalog: &launches(&["vera", "theo"]),
            diagnostics: refusing_diagnostics(&roster, &reasons),
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );

    assert!(
        report.failure.is_none(),
        "a refused person is an expected condition, not a broken plan: {:?}",
        report.failure
    );
    assert_eq!(
        report.refused.get("nix").map(String::as_str),
        Some(GATE_REASON),
        "and they are NAMED, with chiefd's own reason: {:?}",
        report.refused
    );
    assert_eq!(report.steps_ok, 2, "the two launchable people were both applied");
    assert_eq!(report.steps_reached, 3, "and the whole plan was walked");
    let calls = exec.tmux_host().runner().calls();
    let person_split = |person: &str| {
        calls.iter().any(|argv| {
            argv.first().is_some_and(|verb| verb == "split-window")
                && argv.iter().any(|argument| argument == person)
        })
    };
    assert!(person_split("theo"), "the person behind the refusal was started: {calls:?}");
    assert!(!person_split("nix"), "and no pane was minted for the refused person");
}

/// The PROVIDER gate's sentence, which is a different producer of the same
/// refusal from the identity gate's.
///
/// A refusal reaching the interpreter is only a `(person, reason)` pair, and
/// this fixture keeps that honest: the skip must not encode which gate said no.
/// `read_materialized_resources_for_launch` declines a person whose agent home
/// reaches no provider configuration, which is the ORDINARY state of a fresh
/// box — an operator who has not signed Pi in — so it is a far commoner road
/// into this defect than a key rotation that went wrong. Since chief stopped
/// redirecting `PI_CODING_AGENT_DIR` the gate asks this of the OPERATOR's own
/// directory, so it refuses the whole company at once rather than one person.
const PROVIDER_GATE_REASON: &str = "the operator's own Pi agent directory \
    (/root/.pi/agent) reaches no provider configuration: neither auth.json nor models.json is a \
    file there";

/// THE MEASURED REPRO, COMMITTED GREEN.
///
/// The exact scenario `suite-regress` measured against the defect: a plan of
/// two spawn steps whose FIRST person is absent from the launch catalog and
/// whose second is perfectly launchable. Their reading was
///
/// ```text
/// steps_ok=0 steps_total=2
/// failure=Some(Internal { index: 0, detail: "no launch spec for person 'theo'" })
/// ```
///
/// — `vera` was launchable, was second in the plan, and was NEVER ATTEMPTED.
/// The refusal at index 0 is the sharpest form of the defect: nothing had
/// happened yet, and the whole pass was already over.
#[test]
fn a_refusal_at_the_very_first_step_still_starts_the_person_after_it() {
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::then_always([], ScriptedReply::ok("%2\t4243\tcobalt-session"));
    let exec = executor(scripted);
    let desired = desired_two_people_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![
        plan::Step::SplitPane {
            w: plan::WindowRef::Observed("@1".into()),
            spec: plan::SpawnSpec { person_id: "theo".into(), launch_hash: "hash-3".to_owned() },
        },
        plan::Step::SplitPane {
            w: plan::WindowRef::Observed("@1".into()),
            spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        },
    ];
    let roster: BTreeSet<String> =
        ["chief", "theo", "vera"].into_iter().map(str::to_owned).collect();
    // The PROVIDER gate's reason, not the identity gate's: one skip serves both
    // producers, and a test that encoded either one would let the other rot.
    let reasons = BTreeMap::from([("theo".to_owned(), PROVIDER_GATE_REASON.to_owned())]);

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            catalog: &launches(&["vera"]),
            diagnostics: refusing_diagnostics(&roster, &reasons),
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );

    assert!(report.failure.is_none(), "measured as Internal before this fix: {:?}", report.failure);
    assert_eq!(report.steps_ok, 1, "vera was second in the plan and vera was started");
    assert_eq!(
        report.refused.get("theo").map(String::as_str),
        Some(PROVIDER_GATE_REASON),
        "and the gate's own sentence survives whichever gate produced it",
    );
    let calls = exec.tmux_host().runner().calls();
    assert!(
        calls.iter().any(|argv| {
            argv.first().is_some_and(|verb| verb == "split-window")
                && argv.iter().any(|argument| argument == "vera")
        }),
        "the person behind the refusal reached tmux: {calls:?}"
    );
}

/// A CALLER THAT SUPPLIED NO DIAGNOSTICS STILL FAIL-STOPS.
///
/// Only the catalog can say that a gate refused somebody. Without the roster
/// and the reasons, "this person has no launch spec" cannot be told from "the
/// plan and the catalog disagree about who exists", and guessing that it is the
/// first would be inventing a reason for a person nobody can explain. The one
/// production call site always supplies them (`resident.rs`); the plain
/// `apply_plan` wrapper does not, and it is only ever used by tests.
#[test]
fn a_missing_launch_spec_with_no_diagnostics_is_still_an_internal_inconsistency() {
    let scripted = ScriptedTmux::then_always([], ScriptedReply::ok(""));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = empty_observed(true);
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@1".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &BTreeMap::new(), &plan_of(steps));

    assert!(
        matches!(report.failure, Some(StepError::Internal { .. })),
        "no catalog diagnostics means no evidence of a refusal: {:?}",
        report.failure
    );
    assert!(report.refused.is_empty());
}

/// THE GENUINE FAIL-STOP IS NOT WEAKENED.
///
/// A person the catalog never iterated is a different fact: the plan and the
/// catalog disagree about who exists, which is an internal inconsistency and
/// evidence that this pass is wrong about the world. It stops, and the step
/// behind it is not attempted.
///
/// NOTE: this test passes on the reverted tree as well — that is the point. It
/// pins that separating the refusal out did not turn every missing launch spec
/// into a skip.
#[test]
fn a_person_the_catalog_never_iterated_still_stops_the_pass() {
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::then_always([], ScriptedReply::ok(""));
    let exec = executor(scripted);
    let desired = desired_two_people_one_window();
    let observed = empty_observed(true);
    let steps = vec![
        plan::Step::SplitPane {
            w: plan::WindowRef::Observed("@1".into()),
            spec: plan::SpawnSpec { person_id: "ghost".into(), launch_hash: "hash-9".to_owned() },
        },
        plan::Step::SplitPane {
            w: plan::WindowRef::Observed("@1".into()),
            spec: plan::SpawnSpec { person_id: "theo".into(), launch_hash: "hash-3".to_owned() },
        },
    ];
    let roster: BTreeSet<String> = ["vera", "theo"].into_iter().map(str::to_owned).collect();
    let reasons = BTreeMap::new();

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            catalog: &launches(&["vera", "theo"]),
            diagnostics: refusing_diagnostics(&roster, &reasons),
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );

    assert!(
        matches!(report.failure, Some(StepError::Internal { index: 0, .. })),
        "a plan naming somebody the catalog never iterated is broken: {:?}",
        report.failure
    );
    assert!(report.refused.is_empty(), "and it is not reported as a refusal");
    assert_eq!(report.steps_reached, 1, "the step behind the failure was never reached");
}

/// A REFUSED FIRST PERSON TAKES ONLY THEIR OWN WINDOW WITH THEM.
///
/// A window is minted by spawning its first pane, so a refused first person
/// means no window — and every later step naming that window would resolve to
/// `window '...' was referenced before it was created`, an `Internal`, which
/// fail-stops the very pass the skip exists to keep alive. The tail behind a
/// refused window creation is skipped by the same name instead, and the other
/// windows are untouched.
#[test]
fn a_refused_first_person_skips_their_window_without_failing_the_pass() {
    use std::collections::BTreeSet;

    let scripted = ScriptedTmux::then_always([], ScriptedReply::ok(""));
    let exec = executor(scripted);
    let desired = desired_two_people_one_window();
    let observed = empty_observed(true);
    let steps = vec![
        plan::Step::CreateWindowWithSpawn {
            w: plan::WindowSym("eng".into()),
            name: "engineering".into(),
            first: plan::SpawnSpec { person_id: "nix".into(), launch_hash: "hash-9".to_owned() },
        },
        plan::Step::SplitPane {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            spec: plan::SpawnSpec { person_id: "theo".into(), launch_hash: "hash-3".to_owned() },
        },
        plan::Step::ApplyLayout {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            retire_sleeping_notice: true,
            panes: vec![plan::PaneRef::Created("theo".into())],
        },
    ];
    let roster: BTreeSet<String> = ["nix", "theo"].into_iter().map(str::to_owned).collect();
    let reasons = BTreeMap::from([("nix".to_owned(), GATE_REASON.to_owned())]);

    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired,
        &observed,
        super::LaunchInputs {
            catalog: &launches(&["theo"]),
            diagnostics: refusing_diagnostics(&roster, &reasons),
            deferred: &BTreeSet::new(),
        },
        &plan_of(steps),
        super::PassContext::default(),
    );

    assert!(
        report.failure.is_none(),
        "an unbuilt window behind a refusal must not read as a broken plan: {:?}",
        report.failure
    );
    assert_eq!(report.refused.get("nix").map(String::as_str), Some(GATE_REASON));
    assert_eq!(report.steps_reached, 3, "the whole plan was walked");
    assert_eq!(report.steps_ok, 0, "and none of that window's steps could apply");
    let calls = exec.tmux_host().runner().calls();
    assert!(
        calls.iter().all(|argv| argv.first().is_none_or(|verb| verb != "new-window")),
        "no window was minted for a person who cannot start: {calls:?}"
    );
}

#[test]
fn later_layout_failure_reaps_only_panes_created_by_this_apply_attempt() {
    // A later layout failure used to leave both panes from this attempt alive.
    // It must still fail-stop (no following step runs), then guardedly reap
    // both exact newly minted pane ids, never any observed/pre-existing pane.
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""), // step 0 creation precondition: no existing pane for vera
        ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"), // step 0 CreateWindowWithSpawn: new-window
        ScriptedReply::ok(""), // #18 P2: mark-minting on the fresh window
        ScriptedReply::ok(""), // #18 P2: mark-minting on the fresh pane
        ScriptedReply::ok(""), // window tag 1
        ScriptedReply::ok(""), // window tag 2
        ScriptedReply::ok(""), // pane tag 1
        ScriptedReply::ok(""), // pane tag 2
        ScriptedReply::ok(""), // pane tag 3
        ScriptedReply::ok(""), // pane tag 4
        ScriptedReply::ok(""), // #18 P2: clear-minting on the fresh window
        ScriptedReply::ok(""), // #18 P2: clear-minting on the fresh pane
        ScriptedReply::ok(""), // collapsed preference: open
        ScriptedReply::ok(""), // expanded preference: default
        ScriptedReply::ok(""), // rail mint reports no pane
        ScriptedReply::ok(""), // step 1 creation precondition: no existing pane for theo
        ScriptedReply::ok(""), // …and no person pane to split beside
        ScriptedReply::ok("%2\t4243\tcobalt-session"), // step 1 SplitPane
        ScriptedReply::ok(""), // #18 P2: mark-minting on the second pane
        ScriptedReply::ok(""), // second pane tag 1
        ScriptedReply::ok(""), // second pane tag 2
        ScriptedReply::ok(""), // second pane tag 3
        ScriptedReply::ok(""), // second pane tag 4
        ScriptedReply::ok(""), // #18 P2: clear-minting on the second pane
        ScriptedReply::ok("160\t40"), // step 2 ApplyLayout dimensions
        ScriptedReply::ok(""), // the placeholder sweep: no rail panel stands here
        ScriptedReply::ok(""), // the rail probe: no sidebar pane in this window
        ScriptedReply::failed("layout rejected"), // step 2 select-layout FAILS
        ScriptedReply::ok("4243\tcobalt-session\tcobalt\ttheo\thash-3\teng"), // rollback re-verifies %2
        ScriptedReply::ok(""),                                                // rollback kills %2
        ScriptedReply::ok("4242\tcobalt-session\tcobalt\tvera\thash-2\teng"), // rollback re-verifies %1
        ScriptedReply::ok(""),                                                // rollback kills %1
    ]);
    let exec = executor(scripted);
    let desired = desired_two_people_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        // A live, pre-existing pane proves cleanup targets only the exact ids
        // minted by this apply call, never merely a pane the snapshot exposed.
        windows: vec![observed_window("@99", "legacy")],
        panes: vec![observed_pane("%99", "@99", "existing", "7")],
    };
    let steps = vec![
        plan::Step::CreateWindowWithSpawn {
            w: plan::WindowSym("eng".into()),
            name: "engineering".into(),
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        },
        plan::Step::SplitPane {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            spec: plan::SpawnSpec { person_id: "theo".into(), launch_hash: "hash-3".to_owned() },
        },
        plan::Step::ApplyLayout {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            retire_sleeping_notice: true,
            panes: vec![
                plan::PaneRef::Created("vera".into()),
                plan::PaneRef::Created("theo".into()),
            ],
        },
        plan::Step::Retag {
            pane: plan::PaneId("%1".into()),
            person_id: "vera".into(),
            launch_hash: "hash-2".to_owned(),
        },
    ];

    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&["vera", "theo"]),
        &plan_of(steps),
    );
    assert_eq!(report.steps_ok, 2, "only creation steps completed");
    assert!(
        matches!(report.failure, Some(StepError::Tmux { index: 2, ref verb, .. }) if verb == "select-layout")
    );
    let calls = exec.tmux_host().runner().calls();
    let names = verbs(&calls);
    // Retag after the failure never ran; only cleanup's `kill-pane`s follow.
    assert_eq!(
        names.iter().filter(|verb| verb.as_str() == "kill-pane").count(),
        2,
        "calls: {calls:?}"
    );
    let killed: Vec<String> = calls
        .iter()
        .filter(|call| call.first().is_some_and(|verb| verb == "kill-pane"))
        .filter_map(|call| call.last().cloned())
        .collect();
    assert_eq!(killed, vec!["%2", "%1"]);
    assert!(!killed.iter().any(|pane| pane == "%99"), "pre-existing pane must never be reaped");
    assert_eq!(
        names.iter().filter(|verb| verb.as_str() == "set-option").count(),
        16,
        "only the window plus two initial pane tag sequences ran (including their #18 P2 minting \
         markers); Retag after failure must not run. SIXTEEN and not seventeen since Stage 3: \
         the seventeenth was the `@chief_sidebar_gesture` stamp that led the layout's command \
         list, warning rails in OTHER PROCESSES that converge was about to reflow them. There is \
         one rail process now and converge shares it, so the warning is a call into the brain \
         after the pass rather than an option two processes have to agree about: {calls:?}"
    );
}

#[test]
fn failed_one_shot_create_session_mints_nothing_so_there_is_nothing_to_reap() {
    // §2.0(2) ONE SHOT: creation is a single tmux invocation now, so its
    // failure means the SERVER rejected the whole message — no partially-tagged
    // pane can have been minted by us, no rollback probe, no kill-pane.
    let scripted = ScriptedTmux::new([
        ScriptedReply::failed("duplicate session: cobalt-session"), // the one-shot new-session
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &empty_observed(false),
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::CreateSession {
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        }]),
    );
    assert!(
        matches!(report.failure, Some(StepError::Tmux { index: 0, ref verb, .. }) if verb == "new-session")
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(calls.len(), 1, "no follow-up or cleanup call may run: {calls:?}");
}

#[test]
fn create_window_by_move_breaks_and_tags_in_one_tmux_invocation() {
    // §2.0(2) ONE SHOT (F12, architecture-audit Step 2): `break-pane` moves
    // the pane AND mints its new window in one server-side operation; the new
    // window's identity tags ride the same message, addressed via the MOVED
    // pane id (which survives the move and resolves to the fresh window).
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"), // existing move target precondition
        // The watched-window chokepoint: `@1` is not the active window,
        // so the break-pane proceeds exactly as before.
        ScriptedReply::ok("@1\t%old\t0\t\n@7\t%z\t1\t"),
        ScriptedReply::ok("%old\t@2\t4242\tcobalt-session"), // the one-shot break-pane
        ScriptedReply::ok(""),                               // collapsed preference: open
        ScriptedReply::ok(""),                               // expanded preference: default
        ScriptedReply::ok(""),                               // rail mint reports no pane
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng")],
        panes: vec![observed_pane("%old", "@1", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&[]),
        &plan_of(vec![plan::Step::CreateWindowByMove {
            w: plan::WindowSym("new-window".into()),
            name: "new window".into(),
            move_pane: plan::PaneId("%old".into()),
        }]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(
        calls.len(),
        6,
        "the re-verify, the watched-window read, one move, two preference reads, and rail \
         mint: {calls:?}"
    );
    let argv = &calls[2];
    assert_eq!(argv[0], "break-pane");
    assert!(argv.windows(2).any(|pair| pair == ["-s", "%old"]), "argv: {argv:?}");
    assert!(argv.iter().any(|a| a == "new window"), "the new window is named: {argv:?}");
    // No bootstrap pane, no join, no kill: the display-message read follow-on
    // plus exactly two set-option follow-ons.
    assert_eq!(argv.iter().filter(|a| a.as_str() == ";").count(), 3, "argv: {argv:?}");
    assert!(
        argv.windows(2).any(|pair| pair == [";", "display-message"]),
        "the minted identity is read by a same-message display-message follow-on: {argv:?}"
    );
    for expected in ["@organization_id", "cobalt", "@organization_window_id", "new-window"] {
        assert!(argv.iter().any(|a| a == expected), "missing {expected} in argv: {argv:?}");
    }
}

/// Drive one `CreateWindowByMove` against a session whose existing panes answer
/// the rail probe with `existing_rails`, and give back every tmux argv issued.
///
/// `existing_rails` is what `list-panes -s -F '#{@organization_sidebar}'`
/// prints: one line per pane in the session, `1` for a rail. That IS the gate —
/// a company is railed when any window already carries a rail pane — so this
/// argument is the whole condition under test.
///
/// `CreateWindowByMove` is the shortest of the two post-attach mint paths, so
/// it is the one the rail gate is asserted through; the `CreateWindowWithSpawn`
/// path shares the same `ensure_rail_in_window` and the same gate.
fn rail_gate_calls(existing_rails: &str, minted_rail_pane: &str) -> Vec<Vec<String>> {
    let _ = existing_rails;
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // The watched-window chokepoint: `@1` is not the active window,
        // so the break-pane proceeds exactly as before.
        ScriptedReply::ok("@1\t%old\t0\t\n@7\t%z\t1\t"),
        ScriptedReply::ok("%old\t@2\t4242\tcobalt-session"),
        ScriptedReply::ok(""),               // collapsed preference: open
        ScriptedReply::ok(""),               // expanded preference: default
        ScriptedReply::ok(minted_rail_pane), // split-window -P, when it happens
        ScriptedReply::ok(""),               // the rail pane's own tag, when it happens
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng")],
        panes: vec![observed_pane("%old", "@1", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&[]),
        &plan_of(vec![plan::Step::CreateWindowByMove {
            w: plan::WindowSym("new-window".into()),
            name: "new window".into(),
            move_pane: plan::PaneId("%old".into()),
        }]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    exec.tmux_host().runner().calls()
}

#[test]
fn a_window_minted_after_the_attach_gets_its_rail() {
    // THE GAP THIS CLOSES: `attach` swept the windows that existed when the
    // operator arrived, and creates no windows itself. A department that starts
    // afterwards opens a window HERE, and used to open it with no rail at all —
    // the operator's only navigation, missing, with nothing to explain why.
    // Another window of this session already carries a rail, so this is a
    // company that is operated with a rail.
    let calls = rail_gate_calls("\n1\n", "%77");
    let split = calls
        .iter()
        .find(|call| call[0] == "split-window")
        .expect("a railed session mints a rail in the window it just created");
    assert!(split.windows(2).any(|pair| pair == ["-t", "@2"]), "into the NEW window: {split:?}");
    assert!(split.iter().any(|a| a == "-b"), "before the target, i.e. to its LEFT: {split:?}");
    assert!(split.iter().any(|a| a == "-h"), "a horizontal split: {split:?}");
    assert!(split.iter().any(|a| a == "sidebar"), "running the rail: {split:?}");
    assert!(split.iter().any(|a| a == "-c"), "the company is selected by cwd: {split:?}");
    assert_eq!(split.last().map(String::as_str), Some("sidebar"), "bare internal verb: {split:?}");
    assert!(!split.iter().any(|a| a == "cobalt"), "no obsolete company argument: {split:?}");

    let tag = calls
        .iter()
        .find(|call| call[0] == "set-option" && call.contains(&"%77".to_owned()))
        .expect("the minted rail is tagged, or nothing would ever find it again");
    assert!(tag.contains(&"@organization_sidebar".to_owned()), "tagged as a RAIL: {tag:?}");
    assert!(
        !tag.contains(&"@organization_person_id".to_owned()),
        "and never as a person — a rail must not be adopted as one: {tag:?}"
    );
}

#[test]
fn every_company_window_gets_a_rail_without_an_attach_marker() {
    let calls = rail_gate_calls("\n\n", "%77");
    assert!(
        calls.iter().any(|call| call[0] == "split-window"),
        "the rail is required infrastructure, not an attach preference: {calls:?}"
    );
    assert!(calls.iter().any(|call| call[0] == "set-option"), "the required rail is tagged");
}

#[test]
fn closing_every_rail_does_not_disable_rails_for_new_windows() {
    let calls = rail_gate_calls("\n\n\n", "%77");
    assert!(
        calls.iter().any(|call| call[0] == "split-window"),
        "a closed rail is damage to repair, never an off switch: {calls:?}"
    );
}

#[test]
fn one_surviving_rail_keeps_the_company_railed() {
    // The other direction, and the reason the answer is "ANY window" rather
    // than "the window this one was minted beside": closing ONE rail is closing
    // a rail, not turning the feature off.
    let calls = rail_gate_calls("\n\n1\n\n", "%77");
    assert!(
        calls.iter().any(|call| call[0] == "split-window"),
        "one rail left anywhere in the session is still a railed company: {calls:?}"
    );
}

#[test]
fn a_rail_that_cannot_be_minted_never_fails_the_converge_pass() {
    // A window too narrow to hold a rail AND its people refuses the split. The
    // company running matters more than the rail, and the next attach sweep
    // repairs it — so the step must still succeed. `rail_gate_calls` asserts
    // the report succeeded, so an empty mint answer reaching here is the claim.
    let calls = rail_gate_calls("\n1\n", "");
    assert!(calls.iter().any(|call| call[0] == "split-window"), "it tried: {calls:?}");
    assert!(
        !calls.iter().any(|call| call[0] == "set-option"),
        "but tagged nothing, because nothing was minted: {calls:?}"
    );
}

#[test]
fn failed_create_window_by_move_mints_nothing_so_there_is_nothing_to_reap() {
    // §2.0(2) ONE SHOT: with no bootstrap window there is no bootstrap pane to
    // reap; a refused break-pane changed nothing (the pane never left its old
    // window), so cleanup is exactly zero further calls.
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"), // existing move target precondition
        // The watched-window chokepoint: `@1` is not the active window,
        // so the break-pane proceeds exactly as before.
        ScriptedReply::ok("@1\t%old\t0\t\n@7\t%z\t1\t"),
        ScriptedReply::failed("break refused"),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng")],
        panes: vec![observed_pane("%old", "@1", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&[]),
        &plan_of(vec![plan::Step::CreateWindowByMove {
            w: plan::WindowSym("new-window".into()),
            name: "new window".into(),
            move_pane: plan::PaneId("%old".into()),
        }]),
    );
    assert!(
        matches!(report.failure, Some(StepError::Tmux { ref verb, .. }) if verb == "break-pane")
    );
    let calls = exec.tmux_host().runner().calls();
    assert_eq!(
        calls.len(),
        3,
        "the re-verify read, the watched-window read, and the refused one-shot: {calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c[0] == "kill-pane" || c[0] == "new-window"),
        "no bootstrap machinery remains: {calls:?}"
    );
}

#[test]
fn rollback_refuses_a_tagged_pane_whose_spawn_pid_changed() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""), // step 0 creation precondition: no existing pane
        ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // #18 P2: mark-minting (window, pane)
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // complete tags
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // #18 P2: clear-minting (window, pane)
        ScriptedReply::ok(""), // collapsed preference: open
        ScriptedReply::ok(""), // expanded preference: default
        ScriptedReply::ok(""), // rail mint reports no pane
        ScriptedReply::ok("160\t40"),
        ScriptedReply::ok(""), // the placeholder sweep: no rail panel stands here
        ScriptedReply::ok(""), // the rail probe: no sidebar pane in this window
        ScriptedReply::failed("layout rejected"),
        ScriptedReply::ok("9999\tcobalt-session\tcobalt\tvera\thash-2\teng"), // same tags, different process
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &empty_observed(true),
        &launches(&["vera"]),
        &plan_of(vec![
            plan::Step::CreateWindowWithSpawn {
                w: plan::WindowSym("eng".into()),
                name: "engineering".into(),
                first: plan::SpawnSpec {
                    person_id: "vera".into(),
                    launch_hash: "hash-2".to_owned(),
                },
            },
            plan::Step::ApplyLayout {
                w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
                retire_sleeping_notice: true,
                panes: vec![plan::PaneRef::Created("vera".into())],
            },
        ]),
    );
    assert!(
        matches!(report.failure, Some(StepError::Tmux { index: 1, ref verb, .. }) if verb == "select-layout")
    );
    assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"kill-pane".to_owned()));
}

#[test]
fn rollback_refuses_a_tagged_pane_whose_launch_hash_changed_with_the_same_pid() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""), // step 0 creation precondition: no existing pane
        ScriptedReply::ok("%1\t@1\t4242\tcobalt-session"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // #18 P2: mark-minting (window, pane)
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // #18 P2: clear-minting (window, pane)
        ScriptedReply::ok(""), // collapsed preference: open
        ScriptedReply::ok(""), // expanded preference: default
        ScriptedReply::ok(""), // rail mint reports no pane
        ScriptedReply::ok("160\t40"),
        ScriptedReply::ok(""), // the placeholder sweep: no rail panel stands here
        ScriptedReply::ok(""), // the rail probe: no sidebar pane in this window
        ScriptedReply::failed("layout rejected"),
        // Same process and basic identity, but a newer reconcile changed the
        // per-person launch hash. Cleanup must leave that pane alone.
        ScriptedReply::ok("4242\tcobalt-session\tcobalt\tvera\thash-5\teng"),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &empty_observed(true),
        &launches(&["vera"]),
        &plan_of(vec![
            plan::Step::CreateWindowWithSpawn {
                w: plan::WindowSym("eng".into()),
                name: "engineering".into(),
                first: plan::SpawnSpec {
                    person_id: "vera".into(),
                    launch_hash: "hash-2".to_owned(),
                },
            },
            plan::Step::ApplyLayout {
                w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
                retire_sleeping_notice: true,
                panes: vec![plan::PaneRef::Created("vera".into())],
            },
        ]),
    );
    assert!(
        matches!(report.failure, Some(StepError::Tmux { index: 1, ref verb, .. }) if verb == "select-layout")
    );
    assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"kill-pane".to_owned()));
}

#[test]
fn create_window_with_spawn_adopts_an_already_materialized_pane_instead_of_minting_a_duplicate() {
    // The apply-time creation precondition: between observe and apply, a
    // concurrent attended start minted exactly this pane (the measured
    // dual-materialization). The interpreter must adopt it into the bindings —
    // no new-window, no tags — and later steps resolve through the adoption.
    let scripted = ScriptedTmux::new([
        // list-panes answers with vera's pane already live, tagged with THIS
        // organization and person (pane %3 in window @2).
        ScriptedReply::ok("%3\t@2\t0\tcobalt\tvera"),
        ScriptedReply::ok("160\t40"), // ApplyLayout dimensions
        ScriptedReply::ok(""),        // the placeholder sweep: no rail panel stands here
        ScriptedReply::ok(""),        // the rail probe: no sidebar pane in this window
        ScriptedReply::ok(""),        // select-layout
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: Vec::new(),
    };
    let steps = vec![
        plan::Step::CreateWindowWithSpawn {
            w: plan::WindowSym("eng".into()),
            name: "engineering".into(),
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        },
        plan::Step::ApplyLayout {
            w: plan::WindowRef::Created(plan::WindowSym("eng".into())),
            retire_sleeping_notice: true,
            panes: vec![plan::PaneRef::Created("vera".into())],
        },
    ];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    let names = verbs(&calls);
    assert!(
        !names.contains(&"new-window".to_owned()),
        "no second window/pane may be minted: {calls:?}"
    );
    // RETAGGING is a PANE option (`set-option -p`). Said that way rather than
    // as "no set-option at all", which stopped being the same claim once the
    // layout's command list began with the session-scoped gesture stamp — a
    // broader assertion than the rule, failing for a write that tags nothing.
    assert!(
        !calls.iter().any(|c| {
            c.first().is_some_and(|verb| verb == "set-option") && c.iter().any(|word| word == "-p")
        }),
        "an adopted pane is never retagged: {calls:?}"
    );
    let layout = calls
        .iter()
        .find(|c| c.iter().any(|word| word == "select-layout"))
        .expect("the layout still applies");
    assert!(
        layout.contains(&"@2".to_string()),
        "the layout targets the adopted window: {layout:?}"
    );
}

#[test]
fn split_pane_adopts_an_already_materialized_pane_and_skips_the_split() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("%3\t@2\t0\tcobalt\tvera"), // vera already materialized elsewhere
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@3".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    assert!(
        !exec.tmux_host().runner().calls().iter().any(|c| c[0] == "split-window"),
        "no duplicate split may run when the pane already exists"
    );
}

#[test]
fn creation_precondition_spawns_normally_when_the_existing_pane_is_dead_or_another_persons() {
    // Only a LIVE pane tagged with THIS organization and THIS person is
    // adopted: a dead pane, a foreign-org pane, or a different person's pane
    // must not suppress the spawn (the reconcile's normal recovery shape).
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(
            "%3\t@2\t1\tcobalt\tvera\n%4\t@2\t0\trival\tvera\n%5\t@2\t0\tcobalt\ttheo",
        ),
        // The split-target probe: no person pane to take room from, so the
        // split targets the window. The rail is never the target — halving it,
        // even for the frame before ApplyLayout, is the jump the operator sees.
        ScriptedReply::ok(""),
        ScriptedReply::ok("%7\t4242\tcobalt-session"), // the split itself
        ScriptedReply::ok(""),                         // #18 P2: mark-minting on the fresh pane
        ScriptedReply::ok(""),                         // pane tag 1
        ScriptedReply::ok(""),                         // pane tag 2
        ScriptedReply::ok(""),                         // pane tag 3
        ScriptedReply::ok(""),                         // pane tag 4
        ScriptedReply::ok(""),                         // #18 P2: clear-minting on the fresh pane
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@3".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    assert!(
        calls.iter().any(|c| c[0] == "split-window"),
        "dead, foreign, and other-person panes must never suppress the spawn"
    );
}

#[test]
fn retag_is_unconditional_and_writes_all_four_tags() {
    let scripted = ScriptedTmux::always(ScriptedReply::ok(""));
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: Vec::new(),
        panes: vec![observed_pane("%9", "@3", "vera", "2")],
    };
    let steps = vec![plan::Step::Retag {
        pane: plan::PaneId("%9".into()),
        person_id: "vera".into(),
        launch_hash: "hash-2".to_owned(),
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    // No re-verify for Retag; it goes straight to four pane set-options.
    assert!(!verbs(&calls).contains(&"display-message".to_string()));
    let tag_calls = calls.iter().filter(|c| c[0] == "set-option").count();
    assert_eq!(tag_calls, 4);
}

// --- window geometry ------------------------

/// Lay one window out and give back every tmux argv the interpreter issued.
fn layout_calls(dimensions_and_layout: &str, panes: &[&str]) -> Vec<Vec<String>> {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(dimensions_and_layout), // ApplyLayout dimensions read
        // The placeholder sweep: no rail panel stands in this window, so
        // nothing is killed and the layout proceeds. It runs BEFORE the rail
        // probe because an absolute layout string cannot name a pane the
        // topology does not contain — see `close_placeholders`.
        ScriptedReply::ok(""),
        // The rail probe: this window has no sidebar pane, so the layout
        // reserves no column and the geometry below is the un-railed one.
        ScriptedReply::ok(""),
        ScriptedReply::ok(""), // select-layout, if it happens
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@2", "eng")],
        panes: panes
            .iter()
            .enumerate()
            .map(|(index, person)| observed_pane(&format!("%{}", index + 1), "@2", person, "2"))
            .collect(),
    };
    let steps = vec![plan::Step::ApplyLayout {
        w: plan::WindowRef::Observed("@2".into()),
        retire_sleeping_notice: true,
        panes: (1..=panes.len())
            .map(|index| plan::PaneRef::Observed(plan::PaneId(format!("%{index}"))))
            .collect(),
    }];
    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(panes), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    exec.tmux_host().runner().calls()
}

#[test]
fn speculative_layout_keeps_the_exact_sleeping_notice_in_the_layout() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("160\t40\tstale"),
        ScriptedReply::ok("%3\teng"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@2", "eng")],
        panes: vec![observed_pane("%1", "@2", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::ApplyLayout {
            w: plan::WindowRef::Observed("@2".into()),
            panes: vec![plan::PaneRef::Observed(plan::PaneId("%1".into()))],
            retire_sleeping_notice: false,
        }]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    assert!(
        !calls.iter().flatten().any(|arg| arg == "kill-pane"),
        "speculation must not retire the existing notice: {calls:?}"
    );
    let layout = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg == "select-layout"))
        .expect("the changed membership is laid out");
    let layout_text = layout.last().expect("layout string");
    assert!(layout_text.contains(",3"), "notice remains a layout member: {layout:?}");
    assert!(layout_text.contains(",1"), "speculative person is also laid out: {layout:?}");
}

/// A pane the plan does not manage is still GIVEN A CELL.
///
/// # The wedge this pins
///
/// One un-tagged stray pane sat in a live company's window. The planner
/// quarantined it — correct, a stray is skipped and never killed — and the
/// layout was then computed without it, against a window that still held it.
/// `select-layout` answers a short layout string with `have 7 panes but need 6`
/// and the step FAILS, so every pass fail-stopped there and abandoned the spawn
/// steps behind it. Thirteen people sat at `starting` for twenty minutes.
///
/// Quarantine decides that converge does not MANAGE a pane. It cannot decide
/// that tmux stops COUNTING it, and this is where the difference is enforced.
#[test]
fn a_pane_the_plan_does_not_manage_is_still_named_by_the_layout() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("160\t40\tstale"),
        // The window census: the person's pane, and a stray carrying no tags
        // at all — exactly what a quarantined pane looks like from here.
        ScriptedReply::ok("%1\t\n%9\t"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@2", "eng")],
        // %9 is absent here on purpose: an un-adoptable pane never reaches the
        // observed topology, which is the whole reason the plan cannot name it.
        panes: vec![observed_pane("%1", "@2", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::ApplyLayout {
            w: plan::WindowRef::Observed("@2".into()),
            panes: vec![plan::PaneRef::Observed(plan::PaneId("%1".into()))],
            retire_sleeping_notice: false,
        }]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    assert!(
        !calls.iter().flatten().any(|arg| arg == "kill-pane"),
        "a quarantined pane is given a cell, never killed: {calls:?}"
    );
    let layout = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg == "select-layout"))
        .expect("the window is laid out");
    let layout_text = layout.last().expect("layout string");
    assert!(layout_text.contains(",1"), "the managed person is laid out: {layout:?}");
    assert!(
        layout_text.contains(",9"),
        "the unmanaged pane is a layout member too, or tmux rejects the whole string: {layout:?}"
    );
}

#[test]
fn proven_live_layout_retires_the_sleeping_notice_once_before_layout() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("160\t40\tstale"),
        ScriptedReply::ok("%3\teng"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@2", "eng")],
        panes: vec![observed_pane("%1", "@2", "vera", "2")],
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::ApplyLayout {
            w: plan::WindowRef::Observed("@2".into()),
            panes: vec![plan::PaneRef::Observed(plan::PaneId("%1".into()))],
            retire_sleeping_notice: true,
        }]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    let mutation = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg == "kill-pane"))
        .expect("notice retirement and layout share one tmux queue");
    assert_eq!(mutation.iter().filter(|arg| *arg == "kill-pane").count(), 1);
    let kill = mutation.iter().position(|arg| arg == "kill-pane").expect("kill position");
    let layout = mutation.iter().position(|arg| arg == "select-layout").expect("layout position");
    assert!(kill < layout, "the exact notice closes before final layout: {mutation:?}");
    assert!(!mutation.last().expect("layout string").contains(",3"));
}

#[test]
fn real_tmux_keeps_one_notice_across_failed_starts_then_retires_it_after_observation() {
    let socket = Socket(format!("chiefd-notice-preservation-{}", std::process::id()));
    let session = format!("cobalt-notice-preservation-{}", std::process::id());
    let runner = SystemTmuxRunner::default();
    let minted = real_tmux_ok(
        &runner,
        &socket,
        &[
            "new-session",
            "-d",
            "-s",
            &session,
            "-n",
            "Engineering",
            "-x",
            "160",
            "-y",
            "30",
            "-P",
            "-F",
            "#{window_id}\t#{pane_id}",
            "--",
            "sleep",
            "30",
        ],
    );
    let (window, rail) = minted.split_once('\t').expect("window and rail");
    let notice = real_tmux_ok(
        &runner,
        &socket,
        &["split-window", "-h", "-t", window, "-P", "-F", "#{pane_id}", "--", "sleep", "30"],
    );
    real_tmux_ok(
        &runner,
        &socket,
        &[
            "set-option",
            "-t",
            &session,
            "@organization_id",
            "cobalt",
            ";",
            "set-option",
            "-t",
            &session,
            "@chief_sidebar_columns",
            "26",
            ";",
            "set-option",
            "-w",
            "-t",
            window,
            "@organization_id",
            "cobalt",
            ";",
            "set-option",
            "-w",
            "-t",
            window,
            "@organization_window_id",
            "eng",
            ";",
            "set-option",
            "-p",
            "-t",
            &notice,
            "@chief_asleep_for",
            "eng",
            ";",
            "set-option",
            "-p",
            "-t",
            rail,
            "@organization_sidebar",
            "1",
        ],
    );
    let notice_pid = real_tmux_ok(
        &runner,
        &socket,
        &["display-message", "-p", "-t", &notice, "-F", "#{pane_pid}"],
    );
    let exec = executor(SystemTmuxRunner::default());
    let mut desired = desired_one_window();
    desired.session = session.clone();

    for attempt in 0..2 {
        let person = real_tmux_ok(
            &runner,
            &socket,
            &["split-window", "-h", "-t", &notice, "-P", "-F", "#{pane_id}", "--", "sleep", "30"],
        );
        real_tmux_ok(
            &runner,
            &socket,
            &[
                "set-option",
                "-p",
                "-t",
                &person,
                "@organization_id",
                "cobalt",
                ";",
                "set-option",
                "-p",
                "-t",
                &person,
                "@organization_window_id",
                "eng",
                ";",
                "set-option",
                "-p",
                "-t",
                &person,
                "@organization_person_id",
                "vera",
                ";",
                "set-option",
                "-p",
                "-t",
                &person,
                "@organization_launch_hash",
                "hash-2",
            ],
        );
        let observed = plan::ObservedTopology {
            session_exists: true,
            session_organization: "cobalt".into(),
            windows: vec![plan::ObservedWindow {
                tmux_id: window.into(),
                organization_id: "cobalt".into(),
                logical_id: "eng".into(),
                protected_ui: true,
                sleeping_notice: true,
            }],
            panes: vec![observed_pane(&person, window, "vera", "hash-2")],
        };
        let report = apply_plan(
            &exec,
            &socket,
            &desired,
            &observed,
            &launches(&["vera"]),
            &plan_of(vec![plan::Step::ApplyLayout {
                w: plan::WindowRef::Observed(window.into()),
                panes: vec![plan::PaneRef::Observed(plan::PaneId(person.clone()))],
                retire_sleeping_notice: false,
            }]),
        );
        assert!(report.succeeded(), "attempt {attempt}: {:?}", report.failure);
        let rows = real_tmux_ok(
            &runner,
            &socket,
            &[
                "list-panes",
                "-t",
                window,
                "-F",
                "#{pane_id}\t#{pane_pid}\t#{pane_width}\t#{@organization_sidebar}\t#{@chief_asleep_for}",
            ],
        );
        assert_eq!(rows.lines().filter(|row| row.ends_with("\teng")).count(), 1);
        assert!(
            rows.lines().any(|row| row.starts_with(&format!("{notice}\t{notice_pid}\t"))),
            "attempt {attempt}: exact notice process changed: {rows}"
        );
        assert!(
            rows.lines()
                .any(|row| row.starts_with(&format!("{rail}\t")) && row.contains("\t26\t1\t")),
            "attempt {attempt}: rail width changed: {rows}"
        );
        real_tmux_ok(&runner, &socket, &["kill-pane", "-t", &person]);
    }

    let person = real_tmux_ok(
        &runner,
        &socket,
        &["split-window", "-h", "-t", &notice, "-P", "-F", "#{pane_id}", "--", "sleep", "30"],
    );
    real_tmux_ok(
        &runner,
        &socket,
        &[
            "set-option",
            "-p",
            "-t",
            &person,
            "@organization_id",
            "cobalt",
            ";",
            "set-option",
            "-p",
            "-t",
            &person,
            "@organization_window_id",
            "eng",
            ";",
            "set-option",
            "-p",
            "-t",
            &person,
            "@organization_person_id",
            "vera",
            ";",
            "set-option",
            "-p",
            "-t",
            &person,
            "@organization_launch_hash",
            "hash-2",
        ],
    );
    let observed = crate::actuate::observe::observe(
        &exec,
        &socket,
        &session,
        &crate::actuate::ever_observed::EverObserved::new(),
    )
    .expect("real observation");
    assert!(observed.windows[0].sleeping_notice);
    let plan = plan::compute_converge_plan(&desired, &observed).expect("retirement plan");
    assert!(plan
        .steps
        .iter()
        .any(|step| matches!(step, plan::Step::ApplyLayout { retire_sleeping_notice: true, .. })));
    let report = apply_plan(&exec, &socket, &desired, &observed, &launches(&["vera"]), &plan);
    assert!(report.succeeded(), "retire exact notice: {:?}", report.failure);
    let final_rows = real_tmux_ok(
        &runner,
        &socket,
        &[
            "list-panes",
            "-t",
            window,
            "-F",
            "#{pane_id}\t#{pane_width}\t#{@organization_sidebar}\t#{@chief_asleep_for}\t#{@organization_person_id}",
        ],
    );
    assert!(!final_rows.contains(&notice), "exact old notice was not retired: {final_rows}");
    assert_eq!(final_rows.lines().count(), 2, "rail and one person only: {final_rows}");
    assert!(final_rows.lines().any(|row| row.starts_with(&format!("{rail}\t26\t1\t"))));
    assert!(final_rows
        .lines()
        .any(|row| row.starts_with(&format!("{person}\t")) && row.ends_with("\tvera")));

    let settled = crate::actuate::observe::observe(
        &exec,
        &socket,
        &session,
        &crate::actuate::ever_observed::EverObserved::new(),
    )
    .expect("settled observation");
    assert!(plan::compute_converge_plan(&desired, &settled)
        .expect("settled plan")
        .steps
        .is_empty());
    let _ = runner.run(&socket, &TmuxCmd { argv: vec!["kill-server".into()] });
}

/// THE DEFECT, at the interpreter's own seam. The layout is derived from the
/// window's LIVE size, so a 202x44 window is laid out at 202x44 — the 80x24
/// that used to be baked in came from the session being minted unsized, not
/// from this arithmetic. For a single CEO the one pane must fill the whole
/// window: no dead space, no second region.
#[test]
fn a_single_pane_is_laid_out_across_the_whole_live_window() {
    let calls = layout_calls("202\t44\tb25f,80x24,0,0,2", &["vera"]);
    let layout = calls
        .iter()
        // The layout no longer starts its command list: the gesture stamp does,
        // in the same sequence so a rail cannot be resized before it is warned.
        .find(|call| call.iter().any(|word| word == "select-layout"))
        .expect("a stale layout is re-applied")
        .last()
        .expect("select-layout carries the layout string")
        .clone();
    assert!(
        layout.contains("202x44,0,0"),
        "the layout must be computed at the window's live geometry, not 80x24: {layout}"
    );
    assert!(!layout.contains("80x24"), "no trace of the server default survives: {layout}");
}

/// N panes, not just the single-CEO case: a department roster laid out in a
/// 202x44 window is still laid out at 202x44, and every pane is inside it.
#[test]
fn a_multi_pane_window_is_laid_out_at_the_live_window_geometry() {
    let calls = layout_calls("202\t44\tb25f,80x24,0,0,2", &["vera", "theo"]);
    let layout = calls
        .iter()
        // The layout no longer starts its command list: the gesture stamp does,
        // in the same sequence so a rail cannot be resized before it is warned.
        .find(|call| call.iter().any(|word| word == "select-layout"))
        .expect("the layout applies")
        .last()
        .expect("select-layout carries the layout string")
        .clone();
    assert!(layout.contains("202x44,0,0"), "the whole-window geometry leads the layout: {layout}");
    assert!(!layout.contains("80x24"), "no 80x24 region survives for two panes: {layout}");
}

/// Every `select-window` the pass issued, as the window it targeted.
fn selected_windows(calls: &[Vec<String>]) -> Vec<String> {
    calls
        .iter()
        .filter_map(|call| {
            let select = call.iter().position(|arg| arg == "select-window")?;
            call.get(select + 2).cloned()
        })
        .collect()
}

/// Drive one step against `observed`, with tmux answering from `replies`, and
/// give back every argv issued.
fn focus_step_calls(
    observed: &plan::ObservedTopology,
    step: plan::Step,
    replies: Vec<ScriptedReply>,
) -> Vec<Vec<String>> {
    let exec = executor(ScriptedTmux::new(replies));
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        observed,
        &launches(&["vera"]),
        &plan_of(vec![step]),
    );
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    exec.tmux_host().runner().calls()
}

/// AND NEVER ANY OTHER WINDOW. A move between two ordinary department windows
/// is placement, not display — the operator asked for nothing and must be moved
/// nowhere. Without this half, every converge pass would drag an attached
/// operator to whichever window happened to be reconciled last.
#[test]
fn a_pane_moved_into_an_ordinary_window_moves_nobodys_glass() {
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng"), observed_window("@2", "quant")],
        panes: vec![observed_pane("%vera", "@1", "vera", "hash-2")],
    };
    let calls = focus_step_calls(
        &observed,
        plan::Step::MovePane {
            pane: plan::PaneId("%vera".into()),
            to: plan::WindowRef::Observed("@2".into()),
        },
        vec![
            identity_reply("vera", "hash-2"),
            // The source window `@1` is not active, so the move is not deferred.
            ScriptedReply::ok("@1\t%vera\t0\t\n@2\t%other\t1\t"),
            ScriptedReply::ok(""),
        ],
    );
    assert!(
        selected_windows(&calls).is_empty(),
        "an ordinary move is detached, like every other mint in this interpreter: {calls:?}"
    );
}

/// **A JOIN IS A DEPARTURE.** `join-pane` takes the pane out of its current
/// window, and tmux destroys a window with its last pane — so a MOVE can take
/// the operator's glass exactly as a kill can, and it did so silently.
#[test]
fn move_pane_defers_when_it_would_empty_the_window_the_operator_is_watching() {
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng"), observed_window("@2", "quant")],
        panes: vec![observed_pane("%vera", "@1", "vera", "hash-2")],
    };
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // `%vera` is the only pane of `@1`, and `@1` is what the operator is on.
        ScriptedReply::ok("@1\t%vera\t1\t\n@2\t%other\t0\t"),
    ]);
    let exec = executor(scripted);
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::MovePane {
            pane: plan::PaneId("%vera".into()),
            to: plan::WindowRef::Observed("@2".into()),
        }]),
    );
    assert!(report.succeeded(), "a watched window is not an error: {:?}", report.failure);
    assert!(
        !verbs(&exec.tmux_host().runner().calls()).contains(&"join-pane".to_string()),
        "the operator's window is not emptied under them: {:?}",
        exec.tmux_host().runner().calls()
    );
}

/// **BREAK-PANE IS A DEPARTURE TOO**, and on tmux 3.3a a SINGLE-pane source
/// window is RE-PARENTED rather than emptied — this interpreter's own comment
/// records that behaviour for a different reason. Either way the window stops
/// being what the operator was looking at.
#[test]
fn create_window_by_move_defers_when_the_watched_window_is_the_source() {
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng")],
        panes: vec![observed_pane("%old", "@1", "vera", "2")],
    };
    let scripted = ScriptedTmux::new([
        identity_reply("vera", "hash-2"),
        // `@1` is active and `%old` is its only pane: break-pane re-parents it.
        ScriptedReply::ok("@1\t%old\t1\t"),
    ]);
    let exec = executor(scripted);
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        &launches(&[]),
        &plan_of(vec![plan::Step::CreateWindowByMove {
            w: plan::WindowSym("new-window".into()),
            name: "new window".into(),
            move_pane: plan::PaneId("%old".into()),
        }]),
    );
    assert!(report.succeeded(), "a watched window is not an error: {:?}", report.failure);
    assert!(
        !verbs(&exec.tmux_host().runner().calls()).contains(&"break-pane".to_string()),
        "the operator's window is not re-parented under them: {:?}",
        exec.tmux_host().runner().calls()
    );
}

/// **THE DIVERGENCE IS ON THE LINE.**
///
/// Both operator incidents had one signature — the rail's SELECTION naming one
/// person while the window they were LOOKING at showed another — and today
/// that divergence is invisible until somebody reconstructs it from two logs.
/// The deferral now carries both facts, so the next occurrence is a grep.
///
/// It is log context and NEVER a guard input: under #1211 a person who has gone
/// stays selected until the operator clicks elsewhere, so vetoing on selection
/// would make their dead window unreapable for ever — the same starvation
/// `kill_window`'s comment warns about, reached through the other door.
#[test]
fn a_deferral_names_the_selected_person_and_whether_it_is_this_window() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("cobalt\tchief-of-staff"),
        ScriptedReply::ok("\t0"),
        ScriptedReply::ok("@1\t%1\t0\t1\t1\n@4\t%9\t1\t\t1"),
    ]);
    let exec = executor(scripted);
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@4", "chief-of-staff")],
        panes: Vec::new(),
    };
    // NON-VACUOUS: the selection names somebody whose person window is NOT the
    // one being reaped, so `selection_matches_window` has a false to report
    // rather than trivially agreeing with whatever it read.
    let report = super::apply_plan_with_launch_roster(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        super::LaunchInputs {
            catalog: &launches(&["vera"]),
            diagnostics: super::LaunchRosterDiagnostics::default(),
            deferred: &std::collections::BTreeSet::new(),
        },
        &plan_of(vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }]),
        super::PassContext { selected_person: Some("vera".to_owned()), ..Default::default() },
    );
    assert!(report.succeeded(), "a watched window is not an error: {:?}", report.failure);
    assert!(
        !verbs(&exec.tmux_host().runner().calls()).contains(&"kill-window".to_string()),
        "the selection is context, not a second veto — the deferral is still the \
         active-window one: {:?}",
        exec.tmux_host().runner().calls()
    );
}

/// **EVERY STEP THAT CAN TAKE A WINDOW PASSES THROUGH THE CHOKEPOINT.**
///
/// The whole finding behind this guard is that operator-safety lived inside ONE
/// of four destructive steps, so the author of the next one had to remember it.
/// A prose rule does not survive that. This is an EXHAUSTIVE match over
/// `plan::Step`: adding a variant fails to COMPILE until somebody says which
/// side of the line it is on, which is the only form of this rule that a future
/// step cannot skip silently.
///
/// It is deliberately a classification and not a call-graph assertion — the
/// four destructive arms are pinned behaviourally by the deferral tests around
/// this one, and this pin exists so a FIFTH arm cannot appear unclassified.
#[test]
fn every_destructive_step_is_classified_against_the_watched_window_guard() {
    fn takes_a_window(step: &plan::Step) -> bool {
        match step {
            // Destroys or collapses a window: MUST go through
            // `defer_if_operator_watching`.
            plan::Step::KillWindow { .. }
            | plan::Step::KillPane { .. }
            | plan::Step::MovePane { .. }
            | plan::Step::CreateWindowByMove { .. } => true,
            // Creates, replaces or rearranges — nothing the operator is
            // watching disappears. `StopSession` takes the whole session, which
            // is the operator's own explicit stop and not a converge decision.
            plan::Step::StopSession
            | plan::Step::CreateSession { .. }
            | plan::Step::CreateWindowWithSpawn { .. }
            | plan::Step::SplitPane { .. }
            | plan::Step::Respawn { .. }
            | plan::Step::Retag { .. }
            | plan::Step::OrderWindows { .. }
            | plan::Step::ApplyLayout { .. } => false,
        }
    }
    assert!(takes_a_window(&plan::Step::KillPane { pane: plan::PaneId("%1".into()) }));
    assert!(takes_a_window(&plan::Step::KillWindow { w: plan::WindowRef::Observed("@1".into()) }));
    assert!(takes_a_window(&plan::Step::MovePane {
        pane: plan::PaneId("%1".into()),
        to: plan::WindowRef::Observed("@2".into()),
    }));
    assert!(takes_a_window(&plan::Step::CreateWindowByMove {
        w: plan::WindowSym("w".into()),
        name: "n".into(),
        move_pane: plan::PaneId("%1".into()),
    }));
    assert!(!takes_a_window(&plan::Step::StopSession));
    assert!(!takes_a_window(&plan::Step::OrderWindows { order: Vec::new() }));
}

// --- KillWindow: the undesired-window kill and its TOCTOU guards ------------
//
// Separate block, added with the generalized `Step::KillWindow` (the rail-only
// zombie window reap). The apply-time re-reads gate a verb that takes a WINDOW
// and everything in it, so each refusal below is a window that must survive.

/// The happy path: the window is still ours, its logical id names no desired
/// window, and nothing desired lives in it — re-verify, list, then kill.
#[test]
fn kill_window_reverifies_tags_and_occupants_and_then_kills() {
    let scripted = ScriptedTmux::new([
        // Tag re-read: ours, and `chief-of-staff` is not a desired window.
        ScriptedReply::ok("cobalt\tchief-of-staff"),
        // Occupant re-read: one rail pane (no person tag), alive.
        ScriptedReply::ok("\t0"),
        // The FURNITURE re-read: no loading panel and no sleeping notice, so
        // this window is not one the sidebar is holding open.
        ScriptedReply::ok("\t"),
        // The active-window read, now through the shared chokepoint's ONE
        // `list-panes -s`: the operator is on some OTHER window, so the reap is
        // not deferred.
        ScriptedReply::ok("@1\t%1\t1\t1\n@4\t%9\t0\t"),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@4", "chief-of-staff")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    let names = verbs(&exec.tmux_host().runner().calls());
    let tag_read = names.iter().position(|v| v == "display-message").expect("tag re-read");
    let pane_read = names.iter().position(|v| v == "list-panes").expect("occupant re-read");
    // The kill no longer starts its own command list: the gesture stamp does,
    // in the same sequence, so every rail is warned before the window dies and
    // tmux reflows them. Found by SEARCHING the argv rather than by its first
    // word, which is what that change moved.
    let kill = exec
        .tmux_host()
        .runner()
        .calls()
        .iter()
        .position(|call| call.iter().any(|word| word == "kill-window"))
        .expect("the kill itself");
    // The chokepoint reads `list-panes -s` too, so it is the SECOND one — the
    // occupant re-read is the first. Positioned by order rather than by verb
    // name, because both now speak the same one.
    let watched = names
        .iter()
        .enumerate()
        .filter(|(_, v)| *v == "list-panes")
        .nth(1)
        .map(|(at, _)| at)
        .expect("the watched-window read");
    assert!(tag_read < pane_read && pane_read < kill, "verify, then list, then kill: {names:?}");
    assert!(
        pane_read < watched && watched < kill,
        "and the watched-window guard is the LAST question asked before the only \
         destructive command in the step: {names:?}"
    );
}

/// Observation does not trust a partial furniture pane enough to protect its
/// window. The interpreter must still refuse the destructive step when the
/// live furniture marker is present at apply time.
#[test]
fn kill_window_apply_guard_refuses_partial_furniture() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("cobalt\tchief-of-staff"),
        ScriptedReply::ok("\t0"),
        ScriptedReply::ok("vera\t"),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@4", "chief-of-staff")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "furniture is a safe deferral: {:?}", report.failure);
    let calls = exec.tmux_host().runner().calls();
    assert!(
        !calls.iter().flatten().any(|word| word == "kill-window"),
        "apply-time furniture must refuse the kill: {calls:?}"
    );
    // The furniture guard refuses BEFORE the chokepoint is reached. Identified
    // by the chokepoint's OWN format string rather than by counting
    // `list-panes` calls — the occupant re-read speaks the same verb, so a
    // count would pin the wrong thing and would move whenever either read did.
    assert!(
        !calls.iter().flatten().any(|word| word.contains("session_attached")),
        "the furniture guard must stop before the watched-window read: {calls:?}"
    );
}

/// NEVER REAP THE WINDOW THE OPERATOR IS ON, and it is a DEFERRAL rather than a
/// failure.
///
/// This is the guard whose absence killed the ancestor of the person-window
/// design (`a1a7aca9f`). A clicked person was moved out of their department
/// window, the emptied window became undesired, this step destroyed it while the
/// operator was looking at it, and tmux fell back last-used → previous → next.
/// Measured live: every click landed on the CEO.
///
/// `Ok` and not `StepError`, deliberately. Nothing about the world is wrong —
/// the operator is simply looking at it — so the rest of the plan must still
/// run, and it must not re-fail for as long as they keep watching. The next pass
/// reaps once they move on, and `Rail::tidy_selection` is what moves them off a
/// window that has gone stale, so the deferral cannot starve forever.
#[test]
fn kill_window_defers_rather_than_reaping_the_window_the_operator_is_watching() {
    let scripted = ScriptedTmux::new([
        // Both TOCTOU re-reads pass: the window really is ours and really is
        // retired. It is only the operator's attention that stops the kill.
        ScriptedReply::ok("cobalt\tchief-of-staff"),
        ScriptedReply::ok("\t0"),
        // And @4 is the session's active window, read through the shared
        // chokepoint's `list-panes -s`.
        ScriptedReply::ok("@1\t%1\t0\t1\n@4\t%9\t1\t"),
    ]);
    let exec = executor(scripted);
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@4", "chief-of-staff")],
        panes: Vec::new(),
    };
    let report = apply_plan(
        &exec,
        &socket(),
        &desired_one_window(),
        &observed,
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }]),
    );
    assert!(
        report.succeeded(),
        "a watched window is not an error — the pass must complete: {:?}",
        report.failure
    );
    assert!(
        !verbs(&exec.tmux_host().runner().calls()).contains(&"kill-window".to_string()),
        "and the window the operator is looking at survives: {:?}",
        exec.tmux_host().runner().calls()
    );
}

/// TOCTOU: the window's live logical id is DESIRED again (the department came
/// back, or the id was re-used) — the kill must not fire.
#[test]
fn kill_window_toctou_a_window_now_desired_aborts_and_never_kills() {
    // `eng` is the desired window of `desired_one_window()`.
    let scripted = ScriptedTmux::new([ScriptedReply::ok("cobalt\teng")]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@4", "chief-of-staff")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(matches!(report.failure, Some(StepError::Precondition { step: "KillWindow", .. })));
    assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"kill-window".to_string()));
}

/// TOCTOU: a live pane of a DESIRED person is inside the window. The plan's
/// ordering moves desired panes out before this step, so finding one means the
/// world moved between observe and apply — refuse, never kill a person.
#[test]
fn kill_window_toctou_a_desired_persons_pane_inside_aborts_and_never_kills() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok("cobalt\tchief-of-staff"),
        // vera — desired in `desired_one_window()` — is alive in this window.
        ScriptedReply::ok("vera\t0"),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@4", "chief-of-staff")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(matches!(report.failure, Some(StepError::Precondition { step: "KillWindow", .. })));
    assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"kill-window".to_string()));
}

/// TOCTOU: a foreign organization's tag, or no tag at all, refuses — this
/// interpreter never destroys a window it cannot prove is its own retired one.
#[test]
fn kill_window_toctou_a_foreign_or_untagged_window_aborts_and_never_kills() {
    for reply in ["rival\tchief-of-staff", "cobalt\t", "\t"] {
        let scripted = ScriptedTmux::new([ScriptedReply::ok(reply)]);
        let exec = executor(scripted);
        let desired = desired_one_window();
        let observed = plan::ObservedTopology {
            session_exists: true,
            session_organization: "cobalt".into(),
            windows: vec![observed_window("@4", "chief-of-staff")],
            panes: Vec::new(),
        };
        let steps = vec![plan::Step::KillWindow { w: plan::WindowRef::Observed("@4".into()) }];

        let report = apply_plan(
            &exec,
            &socket(),
            &desired,
            &observed,
            &launches(&["vera"]),
            &plan_of(steps),
        );
        assert!(
            matches!(report.failure, Some(StepError::Precondition { step: "KillWindow", .. })),
            "reply {reply:?} must refuse"
        );
        assert!(!verbs(&exec.tmux_host().runner().calls()).contains(&"kill-window".to_string()));
    }
}

// --- the woken sleeper's completion ----------------------------------------
//
// THE ONE PATH THE ACTUATOR MAKES A DISPLAY DECISION ON, and why it must.
// The rail performs an ordinary person click itself. A SLEEPER has no pane to
// move, so the click records the selection, posts the wake and ends. chiefd
// grants it, THIS pass spawns or moves the pane into the window placement
// computed from that same recorded selection — and nothing tmux does emits a
// chiefd changefeed event, so no rail can react to the pane landing. The mover
// issues the completion, synchronously, or the click silently does nothing.
//
// TOMBSTONE, and what changed. An earlier version of this rule was deleted with
// `a1a7aca9f`, when the whole focus-window mechanism came out: it failed live
// because moving a person emptied their department window, the reap killed that
// window under the operator, and tmux fell back to the CEO. The completion was
// never the defect — it was the half that was MISSING from the ancestor before
// it. What is different now is that `placement` retains the empty home as the
// return destination and `kill_window` defers an active-window removal.

/// The desired topology with `vera` shown alone in a window of her own.
fn desired_with_a_focus_window() -> placement::Topology {
    let mut desired = desired_one_window();
    desired.windows.push(placement::Window {
        logical_id: placement::FOCUS_WINDOW_ID.into(),
        name: "Vera".into(),
        panes: vec![placement::Pane {
            person_id: "vera".into(),
            launch_hash: "hash-2".into(),
            order: 0,
        }],
    });
    desired
}

/// Drive one step against `observed` with `desired`, and give back every argv.
fn focus_completion_calls(
    desired: &placement::Topology,
    observed: &plan::ObservedTopology,
    steps: Vec<plan::Step>,
    replies: Vec<ScriptedReply>,
) -> Vec<Vec<String>> {
    let exec = executor(ScriptedTmux::new(replies));
    let report =
        apply_plan(&exec, &socket(), desired, observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);
    exec.tmux_host().runner().calls()
}

#[test]
fn a_pane_moved_into_the_person_window_puts_it_on_the_glass() {
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng"), observed_window("@9", "__focus__")],
        panes: vec![observed_pane("%1", "@1", "vera", "hash-2")],
    };
    let calls = focus_completion_calls(
        &desired_with_a_focus_window(),
        &observed,
        vec![
            plan::Step::MovePane {
                pane: plan::PaneId("%1".into()),
                to: plan::WindowRef::Observed("@9".into()),
            },
            plan::Step::ApplyLayout {
                w: plan::WindowRef::Observed("@9".into()),
                retire_sleeping_notice: true,
                panes: vec![plan::PaneRef::Observed(plan::PaneId("%1".into()))],
            },
        ],
        vec![
            identity_reply("vera", "hash-2"),
            // The watched-window chokepoint: the source window is not the
            // one the operator is on, so the move proceeds.
            ScriptedReply::ok("@1\\t%1\\t0\\t\\n@1\\t%rail\\t0\\t1\\n@9\\t%z\\t1\\t"),
            ScriptedReply::ok(""),
            ScriptedReply::ok("160\t40\tstale"),
            ScriptedReply::ok(""),
            ScriptedReply::ok(""),
            ScriptedReply::ok(""),
        ],
    );
    assert_eq!(
        selected_windows(&calls),
        vec!["@9".to_string()],
        "the window the operator's own recorded gesture put a pane into is selected — \
         without this they watch nothing happen and click the person again: {calls:?}"
    );
}

#[test]
fn a_person_window_minted_by_moving_a_pane_is_selected_too() {
    // The other shape of the same landing: the window did not exist yet, so the
    // pane's move MINTS it. `break-pane` is the move and the mint in one.
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng")],
        panes: vec![observed_pane("%1", "@1", "vera", "hash-2")],
    };
    let calls = focus_completion_calls(
        &desired_with_a_focus_window(),
        &observed,
        vec![
            plan::Step::CreateWindowByMove {
                w: plan::WindowSym(placement::FOCUS_WINDOW_ID.into()),
                name: "Vera".into(),
                move_pane: plan::PaneId("%1".into()),
            },
            plan::Step::ApplyLayout {
                w: plan::WindowRef::Created(plan::WindowSym(placement::FOCUS_WINDOW_ID.into())),
                retire_sleeping_notice: true,
                panes: vec![plan::PaneRef::Observed(plan::PaneId("%1".into()))],
            },
        ],
        vec![
            identity_reply("vera", "hash-2"),
            // The watched-window chokepoint: `@1` is not active, so the
            // break-pane proceeds.
            ScriptedReply::ok("@1\t%1\t0\t\n@7\t%z\t1\t"),
            ScriptedReply::ok("%1\t@9\t4242\tcobalt-session"),
            ScriptedReply::ok(""), // collapsed preference: open
            ScriptedReply::ok(""), // expanded preference: default
            // The rail mint reports no pane in this fixture.
            ScriptedReply::ok(""),
            ScriptedReply::ok("160\t40\tstale"),
            ScriptedReply::ok(""),
            ScriptedReply::ok(""),
            ScriptedReply::ok(""),
        ],
    );
    assert_eq!(
        selected_windows(&calls),
        vec!["@9".to_string()],
        "the freshly minted person window is the one on the glass: {calls:?}"
    );
}

/// A woken person's destination stays hidden until its final layout exists.
/// The select must be the last command in the same tmux sequence as the layout,
/// or the operator sees the placeholder at its old geometry for one frame.
#[test]
fn a_person_window_is_selected_only_after_its_final_layout() {
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@1", "eng"), observed_window("@9", "__focus__")],
        panes: vec![observed_pane("%1", "@1", "vera", "hash-2")],
    };
    let calls = focus_completion_calls(
        &desired_with_a_focus_window(),
        &observed,
        vec![
            plan::Step::MovePane {
                pane: plan::PaneId("%1".into()),
                to: plan::WindowRef::Observed("@9".into()),
            },
            plan::Step::ApplyLayout {
                w: plan::WindowRef::Observed("@9".into()),
                retire_sleeping_notice: true,
                panes: vec![plan::PaneRef::Observed(plan::PaneId("%1".into()))],
            },
        ],
        vec![
            identity_reply("vera", "hash-2"),
            // The watched-window chokepoint: the source window is not the
            // one the operator is on, so the move proceeds.
            ScriptedReply::ok("@1\\t%1\\t0\\t\\n@1\\t%rail\\t0\\t1\\n@9\\t%z\\t1\\t"),
            ScriptedReply::ok(""),
            ScriptedReply::ok("160\t40\tstale"),
            ScriptedReply::ok(""),
            ScriptedReply::ok(""),
            ScriptedReply::ok(""),
        ],
    );

    let visible = calls
        .iter()
        .find(|call| call.iter().any(|arg| arg == "select-window"))
        .expect("the completed wake selects its destination");
    let layout_at = visible
        .iter()
        .position(|arg| arg == "select-layout")
        .expect("the final layout shares the visible command sequence");
    let select_at = visible
        .iter()
        .position(|arg| arg == "select-window")
        .expect("the destination becomes visible");
    assert!(layout_at < select_at, "final layout before visibility: {visible:?}");
    assert_eq!(
        visible.last().map(String::as_str),
        Some("@9"),
        "selection is the last mutation in the final frame: {visible:?}"
    );
    assert_eq!(
        calls.iter().filter(|call| call.iter().any(|arg| arg == "select-window")).count(),
        1,
        "no earlier invocation exposes the placeholder: {calls:?}"
    );
}

/// THE PLACEHOLDER MUST NOT SURVIVE THE LAYOUT THAT REPLACES IT.
///
/// Measured on a live company, five times in nine seconds. `select-layout` is
/// fed an ABSOLUTE layout string enumerating every cell, so tmux refuses it when
/// the window holds a pane the string does not name. The rail's loading panel is
/// exactly such a pane — untagged as a person on purpose, so never in the
/// desired topology and never in `panes`.
///
/// What that produced: converge spawned the woken person as a third pane,
/// computed a two-cell layout for the rail and that person, tmux answered `have
/// 3 panes but need 2`, the step failed, and the interpreter reaped the pane it
/// had just created. The person never arrived, so the panel never closed, so the
/// next pass hit the same wall — and at five failed boots the actuator gave up
/// on them for good. The operator met it as "I clicked the Chief of Staff, it
/// said loading, and nothing ever happened".
#[test]
fn a_spawn_is_a_column_and_never_takes_its_room_from_the_rail() {
    let scripted = ScriptedTmux::new([
        ScriptedReply::ok(""), // precondition: no pane for vera yet
        // The window holds the rail (%1) and one person (%5). The split must
        // take its room from %5.
        ScriptedReply::ok("%1\t1\n%5\t"),
        ScriptedReply::ok("%7\t4242\tcobalt-session"),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
        ScriptedReply::ok(""),
    ]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let observed = plan::ObservedTopology {
        session_exists: true,
        session_organization: "cobalt".into(),
        windows: vec![observed_window("@3", "eng")],
        panes: Vec::new(),
    };
    let steps = vec![plan::Step::SplitPane {
        w: plan::WindowRef::Observed("@3".into()),
        spec: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
    }];

    let report =
        apply_plan(&exec, &socket(), &desired, &observed, &launches(&["vera"]), &plan_of(steps));
    assert!(report.succeeded(), "failure: {:?}", report.failure);

    let calls = exec.tmux_host().runner().calls();
    let split = calls.iter().find(|call| call[0] == "split-window").expect("the split");
    assert!(
        !calls.iter().any(|call| call[0] == "new-session"),
        "ordinary starts, including CEO starts, do not use the sleeper first-paint stage"
    );
    assert!(
        split.contains(&"-h".to_string()),
        "a COLUMN beside the rail, not a row under it: {split:?}"
    );
    assert!(split.contains(&"%5".to_string()), "taking its room from the person pane: {split:?}");
    assert!(
        split.iter().any(|arg| arg.contains("is starting…"))
            && split.iter().any(|arg| arg.contains("exec /usr/bin/env")),
        "the final pane itself paints and then execs Pi: {split:?}"
    );
    for forbidden in ["swap-pane", "join-pane", "respawn-pane", "kill-pane"] {
        assert!(
            !calls.iter().any(|call| call.iter().any(|arg| arg == forbidden)),
            "startup must not create or replace another pane with {forbidden}: {calls:?}"
        );
    }
    assert!(
        !split.contains(&"%1".to_string()),
        "and NEVER from the rail, whose width the operator chose — halving it even for the \
         frame before ApplyLayout is the jump they have been reporting: {split:?}"
    );
}

// ---------------------------------------------------------------------------
// THE INSTRUMENT (the 2026-08-26 start outage): what a failing step records.
//
// The rule these pin: a step failure carries the words of whoever said no, and
// those words are never empty. An actuator that records nothing about what it
// attempted cannot be debugged, which is exactly the condition a live company
// was found in — hundreds of failed rounds, no step, no person, no cause.
// ---------------------------------------------------------------------------

/// tmux refuses with a message: the message IS the cause, verbatim.
#[test]
fn a_refused_step_carries_tmuxs_own_words_as_its_cause() {
    let scripted = ScriptedTmux::new([ScriptedReply::failed("duplicate session: cobalt-session")]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &empty_observed(false),
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::CreateSession {
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        }]),
    );
    let failure = report.failure.expect("the step failed");
    assert!(
        failure.cause().contains("duplicate session: cobalt-session"),
        "tmux's own sentence is the evidence and must survive: {}",
        failure.cause()
    );
    assert!(failure.cause().contains("new-session"), "and it names the verb: {}", failure.cause());
}

/// tmux refuses with NO message at all. The cause is still not empty: the exit
/// status is the one fact available and it is stated. Before this, the detail
/// was `stderr.trim()` and a silent refusal produced a blank cause that reached
/// the operator's card as nothing at all.
#[test]
fn a_silent_tmux_refusal_still_produces_a_cause() {
    let scripted = ScriptedTmux::new([ScriptedReply {
        status: 3,
        stdout: String::new(),
        stderr: String::new(),
    }]);
    let exec = executor(scripted);
    let desired = desired_one_window();
    let report = apply_plan(
        &exec,
        &socket(),
        &desired,
        &empty_observed(false),
        &launches(&["vera"]),
        &plan_of(vec![plan::Step::CreateSession {
            first: plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() },
        }]),
    );
    let failure = report.failure.expect("the step failed");
    let StepError::Tmux { ref detail, .. } = failure else {
        panic!("a nonzero tmux exit is a Tmux step error: {failure:?}");
    };
    assert!(!detail.trim().is_empty(), "a silent refusal may never reduce to an empty detail");
    assert!(detail.contains("status 3"), "the exit status is the fact that is available: {detail}");
    assert!(!failure.cause().trim().is_empty(), "and the cause is never empty: {failure:?}");
}

/// Every step kind names its subject, and no subject is empty. This is the
/// instrument itself: the log line for a failing round is built from these two.
#[test]
fn every_step_names_its_kind_and_its_subject() {
    let spec = plan::SpawnSpec { person_id: "vera".into(), launch_hash: "hash-2".to_owned() };
    let steps = vec![
        plan::Step::StopSession,
        plan::Step::CreateSession { first: spec.clone() },
        plan::Step::CreateWindowWithSpawn {
            w: plan::WindowSym("eng".into()),
            name: "Engineering".into(),
            first: spec.clone(),
        },
        plan::Step::CreateWindowByMove {
            w: plan::WindowSym("eng".into()),
            name: "Engineering".into(),
            move_pane: plan::PaneId("%7".into()),
        },
        plan::Step::SplitPane { w: plan::WindowRef::Observed("@3".into()), spec: spec.clone() },
        plan::Step::MovePane {
            pane: plan::PaneId("%7".into()),
            to: plan::WindowRef::Observed("@3".into()),
        },
        plan::Step::Respawn { pane: plan::PaneId("%7".into()), spec },
        plan::Step::Retag {
            pane: plan::PaneId("%7".into()),
            person_id: "vera".into(),
            launch_hash: "hash-2".to_owned(),
        },
        plan::Step::KillPane { pane: plan::PaneId("%7".into()) },
        plan::Step::KillWindow { w: plan::WindowRef::Observed("@3".into()) },
        plan::Step::OrderWindows { order: vec![plan::WindowRef::Observed("@3".into())] },
        plan::Step::ApplyLayout {
            w: plan::WindowRef::Observed("@3".into()),
            panes: vec![plan::PaneRef::Created("vera".into())],
            retire_sleeping_notice: false,
        },
    ];
    for step in &steps {
        assert!(!step.kind().is_empty(), "every step names its kind");
        assert!(!step.subject().trim().is_empty(), "every step names a subject: {}", step.kind());
    }
    // The three that carry a person say WHO, by name.
    for step in &steps {
        if matches!(
            step,
            plan::Step::CreateSession { .. }
                | plan::Step::CreateWindowWithSpawn { .. }
                | plan::Step::SplitPane { .. }
                | plan::Step::Respawn { .. }
                | plan::Step::Retag { .. }
        ) {
            assert!(
                step.subject().contains("vera"),
                "a step for a person names them: {} / {}",
                step.kind(),
                step.subject()
            );
        }
    }
    // And the ones that act on a window or a pane say WHICH.
    assert!(
        steps[9].subject().contains("@3"),
        "KillWindow names its window: {}",
        steps[9].subject()
    );
    assert!(steps[8].subject().contains("%7"), "KillPane names its pane: {}", steps[8].subject());
}
