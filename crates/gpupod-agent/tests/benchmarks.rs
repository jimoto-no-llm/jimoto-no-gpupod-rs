//! benchmarks public_results の検証。

use chrono::{Duration, Utc};
use gpupod_agent::{public_results, read_results, save_result};
use serde_json::json;

fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

#[test]
fn accepts_well_formed_measurements() {
    let moment = now();
    let value = json!({
        "version": 1,
        "disk": {
            "measured_at": moment.to_rfc3339(),
            "size_bytes": 4096_u64 * 1024 * 1024,
            "method": "fio-sequential-1MiB-qd1",
            "direct_io": true,
            "read_mib_s": 1234.5678,
            "write_mib_s": 987.654321,
        },
        "network": {
            "measured_at": moment.to_rfc3339(),
            "size_bytes": 16 * 1024 * 1024,
            "scope": "central",
            "download_mbps": 100.0004,
            "upload_mbps": 90.9996,
        }
    });
    let clean = public_results(&value, Some(moment));
    let disk = &clean["disk"];
    assert_eq!(disk["size_mib"], json!(4096.0));
    assert_eq!(disk["method"], "fio-sequential-1MiB-qd1");
    assert_eq!(disk["direct_io"], json!(true));
    assert_eq!(disk["read_mib_s"], json!(1234.568));
    assert_eq!(disk["write_mib_s"], json!(987.654));
    assert_eq!(disk["stale"], json!(false));
    assert_eq!(disk["size_bytes"], json!(4096_u64 * 1024 * 1024));

    let network = &clean["network"];
    assert_eq!(network["scope"], "central");
    assert_eq!(network["download_mbps"], json!(100.0));
    assert_eq!(network["upload_mbps"], json!(91.0));
    assert_eq!(network["size_mib"], json!(16.0));
}

#[test]
fn rejects_unknown_or_malformed_fields() {
    let moment = now();

    // バージョンが違うものは丸ごと無効。
    assert_eq!(public_results(&json!({"version": 2}), Some(moment)), json!({}));
    assert_eq!(public_results(&json!({}), Some(moment)), json!({}));
    assert_eq!(public_results(&json!("not an object"), Some(moment)), json!({}));

    let cases = vec![
        // 日時が無い・タイムゾーンが無い
        json!({"version": 1, "disk": {"measured_at": "2024-01-01T00:00:00",
            "size_bytes": 1048576, "method": "fio-sequential-1MiB-qd1",
            "direct_io": true, "read_mib_s": 1.0, "write_mib_s": 1.0}}),
        // 将来の日時
        json!({"version": 1, "disk": {"measured_at": (moment + Duration::minutes(10)).to_rfc3339(),
            "size_bytes": 1048576, "method": "fio-sequential-1MiB-qd1",
            "direct_io": true, "read_mib_s": 1.0, "write_mib_s": 1.0}}),
        // サイズが範囲外
        json!({"version": 1, "disk": {"measured_at": moment.to_rfc3339(),
            "size_bytes": 100, "method": "fio-sequential-1MiB-qd1",
            "direct_io": true, "read_mib_s": 1.0, "write_mib_s": 1.0}}),
        // method / direct_io が違う
        json!({"version": 1, "disk": {"measured_at": moment.to_rfc3339(),
            "size_bytes": 1048576, "method": "other", "direct_io": true,
            "read_mib_s": 1.0, "write_mib_s": 1.0}}),
        json!({"version": 1, "disk": {"measured_at": moment.to_rfc3339(),
            "size_bytes": 1048576, "method": "fio-sequential-1MiB-qd1",
            "direct_io": false, "read_mib_s": 1.0, "write_mib_s": 1.0}}),
        // 数値が範囲外
        json!({"version": 1, "disk": {"measured_at": moment.to_rfc3339(),
            "size_bytes": 1048576, "method": "fio-sequential-1MiB-qd1",
            "direct_io": true, "read_mib_s": 0.0, "write_mib_s": 1.0}}),
        json!({"version": 1, "disk": {"measured_at": moment.to_rfc3339(),
            "size_bytes": 1048576, "method": "fio-sequential-1MiB-qd1",
            "direct_io": true, "read_mib_s": 1e9, "write_mib_s": 1.0}}),
        // scope が central でない
        json!({"version": 1, "network": {"measured_at": moment.to_rfc3339(),
            "size_bytes": 1048576, "scope": "peer", "download_mbps": 1.0,
            "upload_mbps": 1.0}}),
        // そもそも dict でない
        json!({"version": 1, "disk": "broken"}),
    ];
    for case in cases {
        assert_eq!(public_results(&case, Some(moment)), json!({}), "case: {case}");
    }
}

#[test]
fn marks_results_older_than_a_week_stale() {
    let moment = now();
    let value = json!({
        "version": 1,
        "network": {
            "measured_at": (moment - Duration::days(8)).to_rfc3339(),
            "size_bytes": 1048576,
            "scope": "central",
            "download_mbps": 10.0,
            "upload_mbps": 5.0,
        }
    });
    let clean = public_results(&value, Some(moment));
    assert_eq!(clean["network"]["stale"], json!(true));

    let recent = json!({
        "version": 1,
        "network": {
            "measured_at": (moment - Duration::days(1)).to_rfc3339(),
            "size_bytes": 1048576,
            "scope": "central",
            "download_mbps": 10.0,
            "upload_mbps": 5.0,
        }
    });
    let clean = public_results(&recent, Some(moment));
    assert_eq!(clean["network"]["stale"], json!(false));
}

#[test]
fn network_payload_over_the_limit_is_dropped() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let moment = now();
    let valid = json!({
        "version": 1,
        "network": {
            "measured_at": moment.to_rfc3339(),
            "size_bytes": 1048576,
            "scope": "central",
            "download_mbps": 10.0,
            "upload_mbps": 5.0,
        }
    });
    let mut oversized = valid.clone();
    oversized["padding"] = json!("x".repeat(20_000));
    std::fs::write(
        temporary.path().join("benchmarks.json"),
        serde_json::to_vec(&oversized).expect("json"),
    )
    .expect("write");
    assert_eq!(read_results(temporary.path()), json!({}));

    // 上限内ならそのまま読める。
    std::fs::write(
        temporary.path().join("benchmarks.json"),
        serde_json::to_vec(&valid).expect("json"),
    )
    .expect("write");
    let read = read_results(temporary.path());
    assert_eq!(read["network"]["scope"], "central");
}

#[test]
fn invalid_result_keeps_previous_measurement() {
    let temporary = tempfile::tempdir().expect("tempdir");
    let moment = now();
    let measurement = json!({
        "measured_at": moment.to_rfc3339(),
        "size_bytes": 1048576,
        "scope": "central",
        "download_mbps": 10.0,
        "upload_mbps": 5.0,
    });
    save_result(temporary.path(), "network", measurement.clone()).expect("save");
    let read = read_results(temporary.path());
    assert_eq!(read["network"]["download_mbps"], json!(10.0));

    // 不正な結果は拒否され、以前の結果は残る。
    let error = save_result(temporary.path(), "network", json!({"scope": "central"}))
        .expect_err("invalid");
    assert_eq!(error.0, "Invalid benchmark result; previous measurement retained");
    let read = read_results(temporary.path());
    assert_eq!(read["network"]["download_mbps"], json!(10.0));

    // ディスク結果も同じ経路を通る。
    let disk = json!({
        "measured_at": moment.to_rfc3339(),
        "size_bytes": 1048576,
        "method": "fio-sequential-1MiB-qd1",
        "direct_io": true,
        "read_mib_s": 500.0,
        "write_mib_s": 400.0,
    });
    save_result(temporary.path(), "disk", disk).expect("save");
    let read = read_results(temporary.path());
    assert_eq!(read["disk"]["read_mib_s"], json!(500.0));
    // read_results は検証後の公開部分だけを返し、version は含めない。
    assert!(read.get("version").is_none());
    let stored: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(temporary.path().join("benchmarks.json")).expect("read"),
    )
    .expect("parse");
    assert_eq!(stored["version"], json!(1));
}
