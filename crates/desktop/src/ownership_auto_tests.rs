use super::*;
use std::sync::mpsc;
use tempfile::tempdir;

const PROFILE: &str = "org:me";

#[test]
fn consent_is_scoped_persisted_and_independent_of_archive_judging() {
    let dir = tempdir().unwrap();
    let state = sources::SourceState {
        db_gate: Default::default(),
        db_path: dir.path().join("workcontext.sqlite3"),
        config_path: dir.path().join("config.json"),
        config_lock: Default::default(),
        organizer_generation: Default::default(),
    };
    let mut cfg = SettingsFile::default();
    cfg.extra.insert("archiveJudgeOnSync".into(), true.into());
    sources::config::save(&state.config_path, &cfg).unwrap();
    assert!(!authorized_by(&state, PROFILE));
    assert!(super::super::agreed(Ok(PROFILE), PROFILE).is_ok());
    assert!(super::super::agreed(Ok("org:other"), PROFILE).is_err());
    assert!(!authorized_by(&state, PROFILE));
    assert!(set_auto(&state, Ok("org:other"), PROFILE, true, Ok(())).is_err());
    assert!(set_auto(&state, Ok(PROFILE), PROFILE, true, Err("缺少凭据")).is_err());
    set_auto(&state, Ok(PROFILE), PROFILE, true, Ok(())).unwrap();
    assert!(authorized_by(&state, PROFILE));
    assert!(!authorized_by(&state, "org:other"));
    set_auto(&state, Err("无归档"), PROFILE, false, Err("缺少凭据")).unwrap();
    let saved = sources::load_settings(&state).unwrap();
    assert_eq!(authorized(&saved), None);
    assert_eq!(saved.extra["archiveJudgeOnSync"], true);
}

#[test]
fn retry_view_uses_persisted_consent_and_backoff_and_clears_after_manual_success() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let path = state_path(root);
    let state = sources::SourceState {
        db_gate: Default::default(), db_path: root.join("workcontext.sqlite3"),
        config_path: root.join("config.json"), config_lock: Default::default(),
        organizer_generation: Default::default(),
    };
    let view = |profile, running| view_status(root, &state, Some(profile), running);
    set_auto(&state, Ok(PROFILE), PROFILE, true, Ok(())).unwrap();
    let mut schedule = Schedule::load(&path, PROFILE);
    schedule.fail(1_000);
    schedule.save(&path).unwrap();
    assert_eq!(view(PROFILE, false), (true, Some(61_000)));
    assert_eq!(view(PROFILE, true), (true, None));
    assert_eq!(view("org:other", false), (false, None));
    set_auto(&state, Ok(PROFILE), PROFILE, false, Ok(())).unwrap();
    assert_eq!(view(PROFILE, false), (false, None));
    set_auto(&state, Ok("org:other"), "org:other", true, Ok(())).unwrap();
    assert_eq!(view("org:other", false), (true, None));
    set_auto(&state, Ok(PROFILE), PROFILE, true, Ok(())).unwrap();
    let pending = Ticket::open(path.clone(), Schedule::load(&path, PROFILE), Some(Watermark { rowid: 1 }), true).unwrap();
    drop(pending); // 进程退出，调度尚未记录新的重试时间。
    assert_eq!(view(PROFILE, false), (true, None));
    Ticket::open(path.clone(), Schedule::load(&path, PROFILE), None, false).unwrap().settle(false, false);
    assert_eq!(view(PROFILE, false), (true, None));
}

#[test]
fn only_new_rows_in_a_complete_authorized_archive_advance_the_watermark() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("archive")).unwrap();
    let db = Connection::open(root.join("archive/archive.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE messages(mid TEXT PRIMARY KEY, occurred_at INTEGER);
                      CREATE TABLE archive_state(key TEXT PRIMARY KEY, value TEXT);
                      INSERT INTO archive_state VALUES('profile','org:me');").unwrap();
    let mut cfg = SettingsFile::default();
    cfg.extra.insert(CONSENT.into(), PROFILE.into());
    assert!(candidate(root, &cfg, true).is_none());
    db.execute("INSERT INTO messages VALUES('first',10)", []).unwrap();
    assert!(candidate(root, &cfg, false).is_none());
    let (_, first) = candidate(root, &cfg, true).unwrap();
    db.execute("UPDATE messages SET occurred_at=20", []).unwrap();
    assert_eq!(watermark(root, PROFILE), Some(first));
    db.execute("INSERT INTO archive_state VALUES('message_window','partial')", []).unwrap();
    db.execute("INSERT INTO messages VALUES('late',5)", []).unwrap();
    assert!(candidate(root, &cfg, true).is_none());
    db.execute("DELETE FROM archive_state WHERE key='message_window'", []).unwrap();
    assert!(watermark(root, PROFILE).unwrap().rowid > first.rowid);
    db.execute("UPDATE archive_state SET value='org:other' WHERE key='profile'", []).unwrap();
    assert!(candidate(root, &cfg, true).is_none());
}

#[test]
fn restart_recovers_interrupted_work_and_retains_backoff_and_success() {
    let dir = tempdir().unwrap();
    let path = state_path(dir.path());
    let first = Watermark { rowid: 1 };
    drop(Ticket::open(path.clone(), Schedule::load(&path, PROFILE), Some(first), true).unwrap());
    let mut state = Schedule::load(&path, PROFILE);
    assert_eq!(state.blocked(&first, 0), Some(Blocked::Interrupted));
    let mut now = 0;
    for delay in [60_000, 300_000, 900_000, 3_600_000, 3_600_000] {
        state.settle(Some(first), Outcome::Failed, now);
        state.save(&path).unwrap();
        state = Schedule::load(&path, PROFILE);
        now += delay;
        assert_eq!(state.blocked(&first, now - 1), Some(Blocked::Backoff));
        assert_eq!(state.blocked(&first, now), None);
    }
    state.settle(Some(first), Outcome::Done, now);
    state.save(&path).unwrap();
    state = Schedule::load(&path, PROFILE);
    assert_eq!(state.blocked(&first, now), Some(Blocked::Reviewed));
    assert_eq!(state.failures, 0);
    assert_eq!(state.retry_at, None);
    let next = Watermark { rowid: 2 };
    state.settle(Some(next), Outcome::Cancelled, now);
    assert_eq!(state.reviewed, Some(first));
    assert_eq!(state.blocked(&next, now), None);
    assert_eq!(Schedule::load(&path, "org:other").blocked(&first, now), None);
}

#[test]
fn manual_and_automatic_jobs_share_one_slot_and_only_auto_is_cancelled() {
    let dir = tempdir().unwrap();
    let path = state_path(dir.path());
    let worker = Arc::new(OwnershipWorker::default());
    for (trigger, other) in [(Trigger::Auto, Trigger::Manual), (Trigger::Manual, Trigger::Auto)] {
        let (release, receive) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let finish_worker = Arc::clone(&worker);
        assert!(start(&worker, trigger,
            || Ticket::open(path.clone(), Schedule::load(&path, PROFILE), None, false).map(Some),
            move |_| { receive.recv().unwrap(); Ok(()) },
            move |_, _, cancelled| {
                *finish_worker.lock() = RunState::default();
                done.send(cancelled.load(Ordering::Acquire)).unwrap();
            },
        ).unwrap());
        assert!(start(&worker, other, || panic!("occupied slot"), |_| unreachable!(), |_, _, _| unreachable!()).is_err());
        worker.stop_auto();
        release.send(()).unwrap();
        assert_eq!(result.recv_timeout(Duration::from_secs(5)).unwrap(), trigger == Trigger::Auto);
    }
}
