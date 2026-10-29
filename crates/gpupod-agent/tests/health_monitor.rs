//! HealthMonitor の停滞検知。

use std::sync::{Arc, Mutex};

use gpupod_agent::HealthMonitor;

type FakeClock = Arc<Mutex<f64>>;

fn monitor_with(timeout: f64, clock: &FakeClock) -> HealthMonitor {
    let tick = clock.clone();
    let closure = move || *tick.lock().expect("clock");
    HealthMonitor::with_clock(timeout, Arc::new(closure))
}

#[test]
fn healthy_only_after_every_progress_task_beats() {
    let clock: FakeClock = Arc::new(Mutex::new(0.0));
    let monitor = monitor_with(180.0, &clock);

    // 最初は 1 タスクも完了していないので未健康。
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot["healthy"], serde_json::json!(false));
    assert_eq!(snapshot["stalled"], serde_json::json!([]));
    assert_eq!(snapshot["version"], serde_json::json!("development"));
    assert_eq!(snapshot["pid"], serde_json::json!(std::process::id()));
    assert_eq!(snapshot["boot_id"].as_str().expect("boot id").len(), 32);

    let mut monitor = monitor_with(180.0, &clock);
    for name in ["poll", "maintenance", "tunnels"] {
        monitor.begin(name);
        monitor.beat(name);
    }
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot["healthy"], serde_json::json!(true));
    assert_eq!(snapshot["stalled"], serde_json::json!([]));
}

#[test]
fn stalled_progress_is_reported_in_order() {
    let clock: FakeClock = Arc::new(Mutex::new(0.0));
    let mut monitor = monitor_with(30.0, &clock);
    for name in ["poll", "maintenance", "tunnels"] {
        monitor.beat(name);
    }
    assert_eq!(monitor.snapshot()["healthy"], serde_json::json!(true));

    *clock.lock().expect("clock") = 31.0;
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot["healthy"], serde_json::json!(false));
    assert_eq!(
        snapshot["stalled"],
        serde_json::json!(["poll", "maintenance", "tunnels"])
    );
}

#[test]
fn stalled_work_is_reported_and_beat_clears_it() {
    let clock: FakeClock = Arc::new(Mutex::new(10.0));
    let mut monitor = monitor_with(5.0, &clock);
    for name in ["poll", "maintenance", "tunnels"] {
        monitor.beat(name);
    }
    monitor.begin("worker");
    assert_eq!(monitor.snapshot()["stalled"], serde_json::json!([]));

    *clock.lock().expect("clock") = 20.0;
    // 進捗タスクが止まっていればそれも停滞として出る。
    let snapshot = monitor.snapshot();
    assert_eq!(
        snapshot["stalled"],
        serde_json::json!(["poll", "maintenance", "tunnels", "worker"])
    );
    assert_eq!(snapshot["healthy"], serde_json::json!(false));

    // 進捗が戻れば残るのは作業中タスクだけ。
    for name in ["poll", "maintenance", "tunnels"] {
        monitor.beat(name);
    }
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot["stalled"], serde_json::json!(["worker"]));
    assert_eq!(snapshot["healthy"], serde_json::json!(false));

    monitor.beat("worker");
    let snapshot = monitor.snapshot();
    assert_eq!(snapshot["stalled"], serde_json::json!([]));
    assert_eq!(snapshot["healthy"], serde_json::json!(true));
}

#[test]
fn release_identity_can_be_overridden_by_file() {
    // release.json が無い環境では development のまま。
    let monitor = HealthMonitor::new(180.0);
    let identity = monitor.identity();
    assert_eq!(identity["version"], serde_json::json!("development"));
    assert_eq!(identity["sequence"], serde_json::json!(0));
}
