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
        screen_reference: None,
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

#[test]
fn a_paused_room_accepts_a_request_and_keeps_its_scale() {
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
    let settings = room(None).room.unwrap();
    let package = RoomPackage::load(&settings.manifest).unwrap();
    let revision = control.request_room(&settings, &package).unwrap();
    let status = control.try_room_status().unwrap().unwrap();
    assert_eq!(status.requested_revision, revision, "{status:?}");
    assert_ne!(status.applied_revision, revision, "{status:?}");

    let settings = room(Some(1.5)).room.unwrap();
    assert!(control.request_room(&settings, &package).is_err());
}

#[test]
fn a_running_room_applies_the_requested_revision() {
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
    let settings = room(None).room.unwrap();
    let package = RoomPackage::load(&settings.manifest).unwrap();
    let revision = control.request_room(&settings, &package).unwrap();
    control.play(true).unwrap();

    let mut start = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = control.try_room_status().unwrap().unwrap();
        if status.applied_revision == revision {
            break;
        }
        assert_ne!(status.state, RoomState::Failed, "{status:?}");
        assert!(Instant::now() < deadline, "{status:?}");
        if session.submit(&object_frame(start, 4_800)).unwrap() {
            start += 4_800;
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let missing = RoomPackage::load("/nonexistent/room.json").err().unwrap();
    assert!(missing.contains("/nonexistent/room.json"), "{missing}");
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

#[test]
fn room_screen_mapping_moves_only_the_referenced_source() {
    unsafe extern "C" {
        fn adm_scene_stream_room_source_json(
            stream: *const c_void,
            element: u64,
            json: *mut *mut c_char,
        ) -> i32;
    }

    let mut renderer = room(Some(1.0));
    renderer.room.as_mut().unwrap().screens = Some(serde_json::json!({
        "schema": "mradm.screen-config",
        "version": 1,
        "reproduction": {
            "coordinates": "polar", "centre": [0.0, 0.0, 1.0], "width": 90.0, "aspect_ratio": 1.78,
        },
    }).to_string());
    let mut session = Session::new(&Config {
        renderer,
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: 48_000,
    })
    .unwrap();
    session.reset(1, 0).unwrap();
    session.configure(1, 1, &[1, 2], None).unwrap();
    let control = session.control();
    control.play(true).unwrap();

    let (right, front) = 29.0_f32.to_radians().sin_cos();
    let initial = [true, false].map(|referenced| {
        (
            if referenced { 1 } else { 2 },
            ObjectState {
                active: true,
                gain: 1.0,
                position: Some([right, front, 0.0]),
                screen_reference: Some(referenced),
                ..Default::default()
            },
        )
    });
    let samples = [0.01; 1024];
    let planes = [1, 2].map(|element| Plane {
        element,
        samples: &samples,
    });
    let frame = Frame {
        initial: &initial,
        planes: &planes,
        ..object_frame(0, 1024)
    };
    assert!(session.submit(&frame).unwrap());
    session.end(1, 1024).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let phase = control.status().unwrap().phase;
        if phase == Phase::Ended {
            break;
        }
        assert_ne!(phase, Phase::Failed, "{}", control.failure_message());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let [mapped, fixed] = [1, 2].map(|element| {
        let mut json = std::ptr::null_mut();
        let shared = &control.inner;
        // SAFETY: the control owns the stream. The result string is transferred here.
        let code =
            unsafe { adm_scene_stream_room_source_json(shared.stream, element, &raw mut json) };
        assert_eq!(code, 0);
        let text = copy_text(json);
        // SAFETY: the call transferred ownership of the string.
        unsafe { (shared.api.adm_free_string)(json) };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["present"], true);
        let direction = &value["direction"];
        (-direction[1].as_f64().unwrap())
            .atan2(direction[0].as_f64().unwrap())
            .to_degrees()
    });
    assert!((mapped - 45.0).abs() < 1e-4, "{mapped}");
    assert!((fixed - 29.0).abs() < 1e-4, "{fixed}");
}
