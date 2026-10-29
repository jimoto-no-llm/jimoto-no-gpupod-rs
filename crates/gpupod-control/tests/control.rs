//! gpupod-control のサービス層テスト。
//!
//! rusqlite + tempfile で、Python 版 portal/services.py と
//! 同じ挙動（トークンローテーション往復、cohort 割当、
//! 中継ポートの重複なし割当、reserve の競合、sync_node の状態更新）を検証する。

use std::sync::Arc;

use base64::Engine;
use gpupod_control::db::{fmt_dt, now};
use gpupod_control::models::{self, new_uuid_hex};
use gpupod_control::services::{self, ServiceError};
use gpupod_control::{App, Config};
use gpupod_common::Template;
use rusqlite::params;
use serde_json::json;

/// 一時ディレクトリ上のアプリを作る。
fn temp_app() -> (tempfile::TempDir, Arc<App>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config::from_env();
    config.state_dir = dir.path().to_path_buf();
    let app = App::new(config).expect("app");
    (dir, app)
}

/// 認証用のダミー SSH 公開鍵（正しいワイヤー形式）。
fn fake_ssh_key() -> String {
    let mut blob = Vec::new();
    blob.extend_from_slice(&11u32.to_be_bytes());
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&32u32.to_be_bytes());
    blob.extend_from_slice(&[7u8; 32]);
    let encoded = base64::engine::general_purpose::STANDARD.encode(blob);
    format!("ssh-ed25519 {encoded} test@control")
}

/// 承認済みテンプレートの定義（4 TCP ポート）。
fn template_definition() -> serde_json::Value {
    json!({
        "id": "ubuntu-24.04",
        "image": "gpupod/ubuntu:24.04",
        "vendors": ["nvidia", "amd"],
        "ports": ["22/tcp", "8888/tcp", "6006/tcp", "8080/tcp"],
    })
}

/// fingerprint 付きでテンプレートを登録する。
fn install_template(app: &App, definition: &serde_json::Value) -> String {
    let template = Template::from_value(definition.clone()).expect("template");
    let conn = app.db.lock();
    models::upsert_template(&conn, &template.id, definition).expect("upsert");
    template.fingerprint()
}

/// 受付中（online + desired_enabled + accepting）な提供PCを作る。
fn accepting_node(app: &App, name: &str, fingerprint: &str) -> models::NodeRow {
    let (node, _token) = {
        let conn = app.db.lock();
        services::enroll_node(&conn, name).expect("enroll")
    };
    let snapshot = json!({
        "node_id": node.uuid_str(),
        "accepting": true,
        "manual_enabled": true,
        "auto_busy": false,
        "gpus": [
            {"id": "gpu-0", "vendor": "nvidia", "name": "RTX 4090", "backend": "nvidia", "selector": "GPU-0"},
            {"id": "gpu-1", "vendor": "nvidia", "name": "RTX 3090", "backend": "nvidia", "selector": "GPU-1"},
        ],
        "leases": {},
        "templates": {"ubuntu-24.04": fingerprint},
        "resource_pool": null,
        "direct_host": "",
    });
    {
        let conn = app.db.lock();
        conn.execute(
            "UPDATE portal_node SET last_seen = ?2, desired_enabled = 1, snapshot = ?3 WHERE id = ?1",
            params![node.id, fmt_dt(now()), snapshot.to_string()],
        )
        .expect("update node");
        models::get_node(&conn, &node.id).expect("node").expect("node row")
    }
}

/// 利用者を作る。
fn create_user(app: &App) -> i64 {
    let conn = app.db.lock();
    models::insert_user(&conn, "tester", &gpupod_control::auth::hash_password("pw"), false, false, true, "")
        .expect("user")
}

#[test]
fn token_rotation_round_trip() {
    let (_dir, app) = temp_app();
    let (node, token) = {
        let conn = app.db.lock();
        services::enroll_node(&conn, "provider-a").expect("enroll")
    };

    // 初期鍵で認証できる。
    {
        let conn = app.db.lock();
        assert!(services::authenticate_node(&conn, &node.id, &token, true).is_some());
        assert!(services::authenticate_node(&conn, &node.id, "wrong-token", true).is_none());
    }

    let rotation_id = new_uuid_hex();
    let rotation_hyphenated = uuid::Uuid::parse_str(&rotation_id).unwrap().to_string();
    let fresh = "N".repeat(40);

    // 短すぎる交換鍵は拒否。
    {
        let conn = app.db.lock();
        let error = services::prepare_rotation(&conn, &node, &rotation_hyphenated, "short").unwrap_err();
        assert!(matches!(&error, ServiceError::Agent(message) if message == "Invalid replacement credential"));
    }
    // 現在と同じ鍵は "fresh credential" で拒否。
    {
        let conn = app.db.lock();
        let error = services::prepare_rotation(&conn, &node, &rotation_hyphenated, &token).unwrap_err();
        assert!(matches!(&error, ServiceError::Agent(message) if message == "The replacement must be a fresh credential"));
    }
    // 準備は同一ローテーションなら再実行できる。
    {
        let conn = app.db.lock();
        let first = services::prepare_rotation(&conn, &node, &rotation_hyphenated, &fresh).expect("prepare");
        assert_eq!(first["status"], "prepared");
        assert_eq!(first["id"], rotation_hyphenated);
        services::prepare_rotation(&conn, &node, &rotation_hyphenated, &fresh).expect("prepare again");
    }
    // 別のローテーションが並行して来たら拒否。
    {
        let conn = app.db.lock();
        let other = uuid::Uuid::new_v4().to_string();
        let other_token = "M".repeat(40);
        let error = services::prepare_rotation(&conn, &node, &other, &other_token).unwrap_err();
        assert!(matches!(&error, ServiceError::Agent(message) if message == "Another credential rotation is pending"));
    }

    // コミット（Bearer の新鍵）で本鍵が切り替わる。
    {
        let conn = app.db.lock();
        let node = models::get_node(&conn, &node.id).unwrap().unwrap();
        let done = services::commit_rotation(&conn, &node.id, &rotation_hyphenated, &fresh).expect("commit");
        assert_eq!(done["status"], "complete");
        assert_eq!(done["id"], rotation_hyphenated);
        // 同じローテーションの再コミットは冪等。
        let again = services::commit_rotation(&conn, &node.id, &rotation_hyphenated, &fresh).expect("commit again");
        assert_eq!(again["status"], "complete");
    }

    {
        let conn = app.db.lock();
        let node = models::get_node(&conn, &node.id).unwrap().unwrap();
        // 新鍵は常に有効。
        assert!(services::authenticate_node(&conn, &node.id, &fresh, true).is_some());
        // 直前の鍵は 120 秒間だけ有効。
        assert!(services::authenticate_node(&conn, &node.id, &token, true).is_some());
        assert!(services::authenticate_node(&conn, &node.id, &token, false).is_none());
        // 期限切れ直前鍵は無効。
        conn.execute(
            "UPDATE portal_node SET previous_token_until = ?2 WHERE id = ?1",
            params![node.id, fmt_dt(now() - chrono::Duration::seconds(1))],
        )
        .unwrap();
        assert!(services::authenticate_node(&conn, &node.id, &token, true).is_none());
    }

    // 未準備ローテーションのコミットは拒否。
    {
        let conn = app.db.lock();
        let missing = uuid::Uuid::new_v4().to_string();
        let error = services::commit_rotation(&conn, &node.id, &missing, &fresh).unwrap_err();
        assert!(matches!(&error, ServiceError::Agent(message) if message == "Replacement credential was not prepared"));
    }
}

#[test]
fn cohort_release_assignment_is_stable() {
    let (_dir, app) = temp_app();
    let fingerprint = install_template(&app, &template_definition());
    let node = accepting_node(&app, "provider-a", &fingerprint);

    // sha256(str(node.pk))[:8] % 10000 は Python 版と同じ値。
    // ノード ID はランダムなので、固定 UUID の既知値で Python 互換を検証する
    // （python: int(hashlib.sha256(u.encode()).hexdigest()[:8], 16) % 10000 == 6119）。
    assert_eq!(services::cohort_bucket("00000000-0000-0000-0000-000000000001"), 6119);
    // 実ノードでは呼び出しごとに同じ値で、str(uuid)（ハイフン付き）をハッシュする。
    let bucket = services::cohort_bucket(&node.uuid_str());
    assert_eq!(bucket, services::cohort_bucket(&node.uuid_str()));
    assert_ne!(bucket, services::cohort_bucket(&node.id)); // hex 32 桁形式とは異なる（Python はハイフン付きを渡す）

    {
        let conn = app.db.lock();
        assert!(services::assigned_release(&conn, &node).is_none());

        // 無効・別チャネルは対象外。
        models::upsert_release(
            &conn,
            &models::ReleaseRow {
                sequence: 7,
                version: "0.0.1".into(),
                target: "provider/gpupod-agent-0.0.1.tar.gz".into(),
                channel: "stable".into(),
                rollout_percent: 100,
                enabled: false,
            },
        )
        .unwrap();
        models::upsert_release(
            &conn,
            &models::ReleaseRow {
                sequence: 8,
                version: "0.0.2".into(),
                target: "provider/gpupod-agent-0.0.2.tar.gz".into(),
                channel: "candidate".into(),
                rollout_percent: 100,
                enabled: true,
            },
        )
        .unwrap();
        assert!(services::assigned_release(&conn, &node).is_none());

        // 配布率を段階的に上げても cohort は動かない（安定順）。
        for percent in [0, 1, 86, 87, 99, 100] {
            models::upsert_release(
                &conn,
                &models::ReleaseRow {
                    sequence: 10,
                    version: "0.1.0".into(),
                    target: "provider/gpupod-agent-0.1.0.tar.gz".into(),
                    channel: "stable".into(),
                    rollout_percent: percent,
                    enabled: true,
                },
            )
            .unwrap();
            let assigned = services::assigned_release(&conn, &node);
            // Python: bucket < rollout_percent * 100 のとき割当。
            let expected = (percent * 100) as u32 > bucket;
            assert_eq!(assigned.is_some(), expected, "percent={percent} bucket={bucket}");
            if let Some(assignment) = assigned {
                assert_eq!(assignment.sequence, 10);
                assert_eq!(assignment.version, "0.1.0");
            }
        }

        // 同一チャンネルで有効な連番が複数あれば、-sequence 先頭が先に当たる。
        models::upsert_release(
            &conn,
            &models::ReleaseRow {
                sequence: 11,
                version: "0.2.0".into(),
                target: "provider/gpupod-agent-0.2.0.tar.gz".into(),
                channel: "stable".into(),
                rollout_percent: 100,
                enabled: true,
            },
        )
        .unwrap();
        let assigned = services::assigned_release(&conn, &node).expect("assigned");
        assert_eq!(assigned.sequence, 11);
        assert_eq!(assigned.version, "0.2.0");
    }
}

#[test]
fn relay_ports_are_assigned_without_overlap() {
    let (_dir, app) = temp_app();
    let definition = template_definition();
    let fingerprint = install_template(&app, &definition);
    let node = accepting_node(&app, "provider-a", &fingerprint);
    let user = create_user(&app);

    let mut used: Vec<i64> = Vec::new();
    for gpu in ["gpu-0", "gpu-1"] {
        let lease = services::reserve(
            &app.db,
            &app.cfg,
            user,
            &node.uuid_str(),
            gpu,
            "ubuntu-24.04",
            &fake_ssh_key(),
            "relay",
            None,
            None,
        )
        .expect("reserve");
        let conn = app.db.lock();
        let routes = models::routes_of_lease(&conn, &lease.id).expect("routes");
        assert_eq!(routes.len(), 4, "4 TCP ポートぶんの経路を作る");
        for route in routes {
            assert!(
                route.relay_port >= app.cfg.relay_port_min as i64
                    && route.relay_port <= app.cfg.relay_port_max as i64,
                "割当ポートは範囲内"
            );
            assert_ne!(route.relay_port, app.cfg.relay_port as i64, "制御ポートは使わない");
            used.push(route.relay_port);
        }
    }
    let unique: std::collections::BTreeSet<i64> = used.iter().copied().collect();
    assert_eq!(unique.len(), used.len(), "同じポートを 2 回割り当てない");
    assert_eq!(used.len(), 8);

    // 中央中継は貸出ごとに最大 4 ポート。
    let mut wide = template_definition();
    wide["id"] = json!("wide");
    wide["ports"] = json!(["22/tcp", "8888/tcp", "6006/tcp", "8080/tcp", "9000/tcp"]);
    let wide_fingerprint = install_template(&app, &wide);
    {
        let conn = app.db.lock();
        let node_now = models::get_node(&conn, &node.id).unwrap().unwrap();
        let mut snapshot = node_now.snapshot;
        snapshot["templates"]["wide"] = json!(wide_fingerprint);
        let mut gpus = snapshot["gpus"].as_array().cloned().unwrap_or_default();
        gpus.push(json!({
            "id": "gpu-2", "vendor": "nvidia", "name": "RTX 2080",
            "backend": "nvidia", "selector": "GPU-2",
        }));
        snapshot["gpus"] = json!(gpus);
        conn.execute(
            "UPDATE portal_node SET snapshot = ?2 WHERE id = ?1",
            params![node.id, snapshot.to_string()],
        )
        .unwrap();
    }
    let error = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-2",
        "wide",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "中央中継は貸出ごとに最大4ポートです。"));
    // 失敗した貸出行は残らない（Python 版はトランザクションでロールバック）。
    {
        let conn = app.db.lock();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM portal_lease WHERE template_id = 'wide'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn reserve_conflicts_are_rejected() {
    let (_dir, app) = temp_app();
    let fingerprint = install_template(&app, &template_definition());
    let node = accepting_node(&app, "provider-a", &fingerprint);
    let user = create_user(&app);

    let lease = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-0",
        "ubuntu-24.04",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .expect("first reserve");

    // 同一 GPU の 2 件目は Python 版と同じ文言で拒否される。
    let error = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-0",
        "ubuntu-24.04",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "このGPUは貸出中、または後片付け中です。"));

    // one_active_gpu_lease: active の重複は部分 UNIQUE INDEX で弾かれる。
    {
        let conn = app.db.lock();
        let duplicate = models::insert_lease(
            &conn,
            user,
            &node.id,
            "gpu-0",
            "ubuntu-24.04",
            &fake_ssh_key(),
            "relay",
            0,
            0,
        );
        assert!(duplicate.is_err(), "active な重複は制約違反になる");
        // 返却済み（active=0）なら同じ GPU を再度貸せる。
        conn.execute(
            "UPDATE portal_lease SET active = 0, status = 'stopped' WHERE id = ?1",
            params![lease.id],
        )
        .unwrap();
        let reused = models::insert_lease(
            &conn,
            user,
            &node.id,
            "gpu-0",
            "ubuntu-24.04",
            &fake_ssh_key(),
            "relay",
            0,
            0,
        );
        assert!(reused.is_ok(), "返却後は同じ GPU を再利用できる");
    }

    // 未知の GPU / テンプレートでも Python 版と同じ文言。
    let error = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-9",
        "ubuntu-24.04",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "このGPUは貸出中、または後片付け中です。"));
    let error = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-1",
        "unknown-template",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "提供PCと一致する承認済みテンプレートを選択してください。"));

    // 受付停止中は予約できない。
    {
        let conn = app.db.lock();
        conn.execute(
            "UPDATE portal_node SET desired_enabled = 0 WHERE id = ?1",
            params![node.id],
        )
        .unwrap();
    }
    let error = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-1",
        "ubuntu-24.04",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "この提供PCは現在、新規の貸出を受け付けていません。"));
}

#[test]
fn sync_node_updates_lease_state() {
    let (_dir, app) = temp_app();
    let fingerprint = install_template(&app, &template_definition());
    let node = accepting_node(&app, "provider-a", &fingerprint);
    let user = create_user(&app);

    let lease = services::reserve(
        &app.db,
        &app.cfg,
        user,
        &node.uuid_str(),
        "gpu-0",
        "ubuntu-24.04",
        &fake_ssh_key(),
        "relay",
        None,
        None,
    )
    .expect("reserve");
    assert_eq!(lease.status, "queued");
    assert!(lease.active);

    let snapshot = json!({
        "node_id": node.uuid_str(),
        "accepting": true,
        "manual_enabled": true,
        "auto_busy": false,
        "gpus": [
            {"id": "gpu-0", "vendor": "nvidia", "name": "RTX 4090", "backend": "nvidia", "selector": "GPU-0"},
            {"id": "gpu-1", "vendor": "nvidia", "name": "RTX 3090", "backend": "nvidia", "selector": "GPU-1"},
        ],
        "leases": {
            lease.id.clone(): {
                "gpu_id": "gpu-0",
                "template_id": "ubuntu-24.04",
                "status": "running",
                "ports": {"22/tcp": [{"HostPort": "32768"}]},
                "error": "",
            },
        },
        "templates": {"ubuntu-24.04": fingerprint},
        "resource_pool": null,
        "direct_host": "",
    });
    let payload = json!({"state": snapshot, "statistics": {"version": 1}, "results": []});

    let response = services::sync_node(&app.db, &node, &payload).expect("sync");
    assert_eq!(response.relay_bind_host, app.cfg.relay_bind_host);
    assert_eq!(response.commands.len(), 1, "start コマンドが 1 件渡る");
    assert_eq!(response.commands[0].kind, "start");
    assert_eq!(response.routes.len(), 4, "running の経路を返す");
    assert_eq!(response.acknowledged, Vec::<String>::new());

    {
        let conn = app.db.lock();
        let updated = models::get_lease(&conn, &lease.id).unwrap().unwrap();
        assert_eq!(updated.status, "running");
        assert!(updated.active);
        // 同期で snapshot の node_id は更新される。
        let node_now = models::get_node(&conn, &node.id).unwrap().unwrap();
        assert_eq!(
            node_now.snapshot.get("node_id").and_then(|v| v.as_str()),
            Some(node.uuid_str().as_str())
        );
    }

    // start の結果を返すとコマンドは done になり acknowledged に入る。
    let command_id = {
        let conn = app.db.lock();
        models::pending_commands(&conn, &node.id, 20).unwrap()[0].id.clone()
    };
    let with_result = json!({
        "state": snapshot,
        "statistics": {},
        "results": [{"id": models::hex_to_uuid(&command_id), "ok": true}],
    });
    let response = services::sync_node(&app.db, &node, &with_result).expect("sync with result");
    assert_eq!(response.acknowledged, vec![models::hex_to_uuid(&command_id)]);
    assert!(response.commands.is_empty(), "完了したコマンドは渡さない");
    {
        let conn = app.db.lock();
        assert!(models::pending_commands(&conn, &node.id, 20).unwrap().is_empty());
    }

    // GPU が一致しない場合は identity mismatch。
    let mut mismatch = snapshot.clone();
    mismatch["leases"][lease.id.clone()]["gpu_id"] = json!("gpu-7");
    let error =
        services::sync_node(&app.db, &node, &json!({"state": mismatch, "results": []})).unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "Lease identity mismatch"));

    // stopped を受けたら active 解除と経路削除が行われる。
    let mut stopped = snapshot.clone();
    stopped["leases"][lease.id.clone()]["status"] = json!("stopped");
    let response =
        services::sync_node(&app.db, &node, &json!({"state": stopped, "results": []})).expect("sync stopped");
    assert!(response.routes.is_empty(), "stopped の経路は返さない");
    {
        let conn = app.db.lock();
        let updated = models::get_lease(&conn, &lease.id).unwrap().unwrap();
        assert_eq!(updated.status, "stopped");
        assert!(!updated.active);
        assert!(models::routes_of_lease(&conn, &lease.id).unwrap().is_empty());
    }

    // 停止済み貸出がスナップショットに無くても保持される（欠落は証拠にならない）。
    let mut empty_state = snapshot.clone();
    empty_state["leases"] = json!({});
    let response =
        services::sync_node(&app.db, &node, &json!({"state": empty_state, "results": []})).expect("sync empty");
    assert!(response.routes.is_empty());

    // 不正な state は Invalid payload / Invalid node snapshot。
    let error = services::sync_node(&app.db, &node, &json!({"results": []})).unwrap_err();
    assert!(matches!(error, ServiceError::Payload));
    let error = services::sync_node(
        &app.db,
        &node,
        &json!({"state": {"node_id": node.uuid_str()}, "results": []}),
    )
    .unwrap_err();
    assert!(matches!(&error, ServiceError::Agent(message) if message == "Invalid node snapshot"));
}
