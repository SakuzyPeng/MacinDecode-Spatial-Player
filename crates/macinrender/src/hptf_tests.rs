use super::*;
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn slow_profile_io_leaves_submission_and_output_controls_live() {
    let mut session = Session::new(&Config {
        renderer: RendererSettings {
            speaker_renderer: SpeakerRenderer::SafVbap,
            binaural: true,
            layout: "4+7+0".into(),
            sofa: String::new(),
            split_lfe: true,
            room: None,
        },
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: 48_000,
    })
    .unwrap();
    session.reset(1, 0).unwrap();
    session.configure(1, 1, &[7], None).unwrap();
    let control = session.control();
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("slow.txt");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (opened, ready) = mpsc::channel();
    let (release, permission) = mpsc::channel();
    let writer_path = fifo.clone();
    let writer = std::thread::spawn(move || {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(writer_path)
            .unwrap();
        opened.send(()).unwrap();
        let _ = permission.recv_timeout(Duration::from_secs(10));
        file.write_all(b"Preamp: -3 dB\n").unwrap();
    });
    let loader = control.clone();
    let prepare = std::thread::spawn(move || {
        loader.set_hptf(
            &HptfSettings {
                profile: fifo.to_str().unwrap().to_owned(),
                right: None,
                auto_trim: false,
            },
            1,
        )
    });
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    let (finished, progress) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        control.status().unwrap();
        control.hptf_status().unwrap();
        control.volume(0.5).unwrap();
        control.play(false).unwrap();
        control.orientation([0.0; 3]).unwrap();
        let samples = [0.0; 480];
        assert!(
            session
                .submit(&Frame {
                    epoch: 1,
                    generation: 1,
                    start: 0,
                    duration: 480,
                    complete: true,
                    planes: &[Plane {
                        element: 7,
                        samples: &samples
                    }],
                    initial: &[(
                        7,
                        ObjectState {
                            active: true,
                            gain: 1.0,
                            position: Some([0.0, 1.0, 0.0]),
                            head_locked: false,
                            screen_reference: None,
                        }
                    )],
                    updates: &[],
                })
                .unwrap()
        );
        finished.send(()).unwrap();
    });
    // A barrier, not a file-size/timing guess: no profile bytes are available
    // until every producer/control call has had a chance to complete.
    let completed_while_reading = progress.recv_timeout(Duration::from_secs(2)).is_ok();
    release.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(prepare.join().unwrap(), Ok(true));
    producer.join().unwrap();
    assert!(
        completed_while_reading,
        "profile I/O blocked the producer or output controls"
    );
}

fn null_binaural() -> Session {
    Session::new(&Config {
        renderer: RendererSettings {
            speaker_renderer: SpeakerRenderer::SafVbap,
            binaural: true,
            layout: "4+7+0".into(),
            sofa: String::new(),
            split_lfe: true,
            room: None,
        },
        output: OutputKind::Null,
        device_id: String::new(),
        input_rate: 48_000,
    })
    .unwrap()
}

fn peak(gain_db: f64) -> HptfBand {
    HptfBand {
        kind: HptfBandKind::Peaking,
        enabled: true,
        fc_hz: 1000.0,
        gain_db,
        q: 1.0,
    }
}

#[test]
fn each_ear_runs_its_own_profile_in_one_revision() {
    let directory = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = directory.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let one = write(
        "one.txt",
        "Preamp: -2 dB\nFilter 1: ON PK Fc 1000 Hz Gain -3 dB Q 1\n",
    );
    let two = write(
        "two.txt",
        "Preamp: -4 dB\nFilter 1: ON PK Fc 100 Hz Gain 2 dB Q 1\nFilter 2: ON PK Fc 5000 Hz Gain -2 dB Q 2\n",
    );
    let mut session = null_binaural();
    let control = session.control();
    let pair = HptfSettings {
        profile: one.clone(),
        right: Some(two.clone()),
        auto_trim: false,
    };
    assert_eq!(control.set_hptf(&pair, 1), Ok(true));
    session.reset(1, 0).unwrap();
    let status = control.hptf_status().unwrap();
    assert_eq!(status.applied_revision, 1);
    assert_eq!((status.left.bands, status.right.bands), (1, 2));
    assert!((status.left.preamp_db + 2.0).abs() < 1e-4);
    assert!((status.right.preamp_db + 4.0).abs() < 1e-4);

    // A broken right ear refuses the pair and leaves the previous one running,
    // left ear included.
    let broken = HptfSettings {
        profile: two,
        right: Some(write("broken.txt", "Filter 1: ON XX Fc 1 Hz\n")),
        auto_trim: false,
    };
    assert!(control.set_hptf(&broken, 2).is_err());
    session.reset(2, 0).unwrap();
    let status = control.hptf_status().unwrap();
    assert_eq!(status.applied_revision, 1);
    assert_eq!((status.left.bands, status.right.bands), (1, 2));

    // Both ears on one file is the shared path, and the per-ear query still
    // answers it.
    let shared = HptfSettings {
        profile: one,
        right: None,
        auto_trim: false,
    };
    assert_eq!(control.set_hptf(&shared, 3), Ok(true));
    session.reset(3, 0).unwrap();
    let status = control.hptf_status().unwrap();
    assert_eq!(status.applied_revision, 3);
    assert_eq!(status.left, status.right);
}

#[test]
fn in_memory_ears_share_one_trim() {
    let mut session = null_binaural();
    let control = session.control();
    let boosted = HptfProfile {
        preamp_db: 0.0,
        bands: vec![peak(6.0)],
    };
    let bare = HptfProfile::default();
    assert_eq!(
        control.set_hptf_stereo_parameters(&boosted, &bare, true, 1),
        Ok(true)
    );
    session.reset(1, 0).unwrap();
    let status = control.hptf_status().unwrap();
    assert_eq!(status.applied_revision, 1);
    assert_eq!((status.left.bands, status.right.bands), (1, 0));
    // The trim the boosted ear needs is taken by both, so an ear with no
    // cascade of its own is still attenuated, and still counts as filtered.
    assert!(status.left.auto_trim_db < -5.0);
    assert!((status.left.auto_trim_db - status.right.auto_trim_db).abs() < 1e-6);
    assert!(status.right.enabled && status.enabled());

    assert_eq!(
        control.set_hptf_stereo_parameters(&bare, &bare, false, 2),
        Ok(true)
    );
    session.reset(2, 0).unwrap();
    assert!(!control.hptf_status().unwrap().enabled());
}
