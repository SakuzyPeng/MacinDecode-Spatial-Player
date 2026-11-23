//! The room renderer through the player's own contract, on the timer-driven
//! null output. The room is the synthetic package `MacinRender` tests with:
//! small, 48 kHz, and binding its own HRIR.
use super::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn fixture(name: &str) -> String {
    let path: PathBuf = [
        env!("MACINRENDER_CORE_SOURCE"),
        "tests/fixtures/room",
        name,
        "room.json",
    ]
    .iter()
    .collect();
    assert!(path.is_file(), "{} is missing", path.display());
    path.to_str().unwrap().to_owned()
}

fn room(metres_per_unit: Option<f64>) -> RendererSettings {
    RendererSettings {
        binaural: true,
        speaker_renderer: SpeakerRenderer::SafVbap,
        layout: "4+7+0".into(),
        sofa: String::new(),
        split_lfe: false,
        room: Some(RoomSettings {
            manifest: fixture("synthetic"),
            hrir: String::new(),
            metres_per_unit,
            screens: None,
        }),
    }
}

#[test]
fn a_room_session_plays_the_player_s_objects_and_lfe() {
    for scale in [None, Some(1.5)] {
        probe_player_renderer(&room(scale), 48_000).unwrap();
    }
    // The room renders at its package's rate and resamples nothing.
    let error = probe_player_renderer(&room(None), 44_100).unwrap_err();
    assert!(!error.is_empty());
}

fn object_frame(start: i64, duration: u32) -> Frame<'static> {
    const OBJECT: ObjectState = ObjectState {
        active: true,
        gain: 1.0,
        position: Some([0.0, 1.0, 0.0]),
        head_locked: false,
    };
    Frame {
        epoch: 1,
        generation: 1,
        start,
        duration,
        complete: true,
        planes: &[Plane {
            element: 1,
            samples: &[],
        }],
        initial: &[(1, OBJECT)],
        updates: &[],
    }
}

/// Feed silence until `done` finishes, the way the producer thread does.
fn produce_until<T>(session: &mut Session, done: &std::thread::JoinHandle<T>) -> i64 {
    const CHUNK: u32 = 4_800;
    let mut start = 0_i64;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done.is_finished() && Instant::now() < deadline {
        if session.submit(&object_frame(start, CHUNK)).unwrap() {
            start += i64::from(CHUNK);
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    start
}

#[test]
fn a_paused_room_accepts_a_switch_without_waiting_for_playback() {
    let mut session = Session::new(&Config {
        renderer: room(None),
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: 48_000,
    })
    .unwrap();
    session.reset(1, 0).unwrap();
    session.configure(1, 1, &[1], None).unwrap();
    let control = session.control();
    let started = Instant::now();
    control.switch_renderer(&room(None)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    let status = control.try_room_status().unwrap().unwrap();
    assert_eq!(status.requested_revision, 1, "{status:?}");
    // A session fixes its scale: only the package may change under it.
    assert!(control.switch_renderer(&room(Some(1.5))).is_err());
}

#[test]
fn a_running_room_takes_another_package_and_refuses_a_bad_one() {
    let mut session = Session::new(&Config {
        renderer: room(None),
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: 48_000,
    })
    .unwrap();
    let control = session.control();
    let status = control.try_room_status().unwrap().expect("a room session");
    assert_eq!(status.state, RoomState::Applied, "{status:?}");

    session.reset(1, 0).unwrap();
    session.configure(1, 1, &[1], None).unwrap();
    control.play(true).unwrap();
    let other = control.clone();
    let worker = std::thread::spawn(move || {
        let switched = other.switch_renderer(&room(None));
        let mut missing = room(None);
        missing.room.as_mut().unwrap().manifest = "/nonexistent/room.json".into();
        (switched, other.switch_renderer(&missing))
    });
    let produced = produce_until(&mut session, &worker);
    let (switched, missing) = worker.join().unwrap();
    switched.unwrap();
    // While it renders, the switch returns once the new room is the one heard.
    let status = control.try_room_status().unwrap().unwrap();
    assert!(status.applied_revision >= 1, "{status:?}");
    assert!(produced > 0);
    let missing = missing.unwrap_err();
    assert!(missing.contains("/nonexistent/room.json"), "{missing}");
    // The room that was playing keeps playing.
    assert_ne!(control.status().unwrap().phase, Phase::Failed);
}

#[test]
fn a_saf_session_renders_no_room() {
    let mut saf = room(None);
    saf.room = None;
    let session = Session::new(&Config {
        renderer: saf,
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: 48_000,
    })
    .unwrap();
    assert_eq!(session.control().try_room_status().unwrap(), None);
}
