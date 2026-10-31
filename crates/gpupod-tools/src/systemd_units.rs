//! systemd ユニット生成（Python 版 systemd_units.py の移植）。
//!
//! 生成文言は Python 版とバイト一致する。ExecStart は Python 版と同じ
//! `.venv/bin/python` 前提で組み立てる（Rust バイナリ前提への切替は README 参照）。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Result};

/// word 単位の引用符付け。改行・CR・% を含むパスは拒否する。
pub fn quote(value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') || value.contains('%') {
        bail!("Unsupported service path");
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

fn valid_user(user: &str) -> bool {
    let bytes = user.as_bytes();
    let first = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_');
    first
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

/// ユニット文面の生成。role は control / provider / updater。
pub fn generate(
    project: &Path,
    state: &Path,
    config: &Path,
    user: &str,
    role: &str,
) -> Result<BTreeMap<String, String>> {
    if !valid_user(user) {
        bail!("Invalid Linux service user");
    }
    if !matches!(role, "control" | "provider" | "updater") {
        bail!("Unknown service role");
    }
    let python = quote(&path_text(&project.join(".venv/bin/python")))?;
    let common = format!(
        "[Unit]\nAfter=network-online.target\nWants=network-online.target\n\n\
         [Service]\nType=simple\nUser={user}\nWorkingDirectory={}\nUMask=0077\n\
         Restart=on-failure\nRestartSec=5\nTimeoutStopSec=30\n",
        path_text(project)
    );
    let suffix = "\n[Install]\nWantedBy=multi-user.target\n";
    let mut units = BTreeMap::new();
    if role == "updater" {
        let oneshot = common
            .replace("Type=simple", "Type=oneshot")
            .replace("Restart=on-failure\nRestartSec=5\n", "");
        units.insert(
            "gpupod-update.service".to_string(),
            oneshot.clone()
                + "TimeoutStartSec=900\n"
                + &format!(
                    "ExecStart={python} helpers/update_agent.py --config {}\n",
                    quote(&path_text(config))?
                ),
        );
        units.insert(
            "gpupod-update.timer".to_string(),
            "[Unit]\nDescription=Periodic provider updates\n\n\
             [Timer]\nOnBootSec=2m\nOnUnitActiveSec=1h\nRandomizedDelaySec=10m\n\
             Persistent=true\nUnit=gpupod-update.service\n\n\
             [Install]\nWantedBy=timers.target\n"
                .to_string(),
        );
        return Ok(units);
    }
    if role == "provider" {
        let supervised = common.replace("Type=simple", "Type=notify")
            + "NotifyAccess=main\nWatchdogSec=45\nWatchdogSignal=SIGKILL\nTimeoutStartSec=180\n";
        units.insert(
            "gpupod-provider.service".to_string(),
            supervised
                + &format!(
                    "ExecStart={python} -m provider_agent --state-dir {} daemon --config {}\n",
                    quote(&path_text(state))?,
                    quote(&path_text(config))?
                )
                + suffix,
        );
        units.insert(
            "gpupod-auto.service".to_string(),
            common.clone()
                + &format!(
                    "ExecStart={python} helpers/auto_lend.py --state-dir {} --mode drain\n",
                    quote(&path_text(state))?
                )
                + suffix,
        );
        return Ok(units);
    }
    let environment = format!(
        "Environment=GPUPOD_CONTROL_STATE={}\nEnvironmentFile=-{}\n",
        quote(&path_text(state))?,
        path_text(&state.join("control.env"))
    );
    units.insert(
        "gpupod-web.service".to_string(),
        common.clone()
            + &environment
            + &format!(
                "ExecStart={} control_panel.wsgi:application --bind 127.0.0.1:8000 \
                 --workers 2 --threads 2 --timeout 30\n",
                quote(&path_text(&project.join(".venv/bin/gunicorn")))?
            )
            + suffix,
    );
    units.insert(
        "gpupod-relay.service".to_string(),
        common + &environment + &format!("ExecStart={python} manage.py runrelay\n",) + suffix,
    );
    Ok(units)
}

/// ファイルへ書き出し、出力パスを返す。
pub fn write_units(
    units: &BTreeMap<String, String>,
    output_dir: &Path,
) -> Result<Vec<std::path::PathBuf>> {
    std::fs::create_dir_all(output_dir)?;
    let mut written = Vec::new();
    for (name, content) in units {
        let path = output_dir.join(name);
        std::fs::write(&path, content)?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn paths() -> (PathBuf, PathBuf, PathBuf) {
        (
            PathBuf::from("/srv/gpupod"),
            PathBuf::from("/var/lib/gpupod"),
            PathBuf::from("/srv/gpupod/.provider-state/node.secret.json"),
        )
    }

    #[test]
    fn quote_escapes_and_rejects() {
        assert_eq!(quote("/plain/path").unwrap(), "\"/plain/path\"");
        assert_eq!(quote("/with space/x").unwrap(), "\"/with space/x\"");
        assert_eq!(quote("/a\"b").unwrap(), "\"/a\\\"b\"");
        assert_eq!(quote("/back\\slash").unwrap(), "\"/back\\\\slash\"");
        assert!(quote("/bad\npath").is_err());
        assert!(quote("/bad%path").is_err());
        assert!(quote("/bad\rpath").is_err());
    }

    #[test]
    fn provider_units_match_the_python_layout() {
        let (project, state, config) = paths();
        let units = generate(&project, &state, &config, "gpupod", "provider").unwrap();
        assert_eq!(
            units["gpupod-provider.service"],
            "[Unit]\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=notify\nUser=gpupod\nWorkingDirectory=/srv/gpupod\nUMask=0077\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=30\nNotifyAccess=main\nWatchdogSec=45\nWatchdogSignal=SIGKILL\nTimeoutStartSec=180\nExecStart=\"/srv/gpupod/.venv/bin/python\" -m provider_agent --state-dir \"/var/lib/gpupod\" daemon --config \"/srv/gpupod/.provider-state/node.secret.json\"\n\n[Install]\nWantedBy=multi-user.target\n"
        );
        assert!(units["gpupod-provider.service"].contains("Type=notify"));
        assert!(!units["gpupod-provider.service"].contains("Type=simple"));
        assert!(units["gpupod-auto.service"]
            .contains("helpers/auto_lend.py --state-dir \"/var/lib/gpupod\" --mode drain"));
    }

    #[test]
    fn control_units_carry_environment() {
        let (project, state, config) = paths();
        let units = generate(&project, &state, &config, "gpupod", "control").unwrap();
        let web = &units["gpupod-web.service"];
        assert!(web.contains("Environment=GPUPOD_CONTROL_STATE=\"/var/lib/gpupod\""));
        assert!(web.contains("EnvironmentFile=-/var/lib/gpupod/control.env"));
        assert!(web.contains(
            "ExecStart=\"/srv/gpupod/.venv/bin/gunicorn\" control_panel.wsgi:application"
        ));
        assert!(units["gpupod-relay.service"]
            .contains("ExecStart=\"/srv/gpupod/.venv/bin/python\" manage.py runrelay"));
    }

    #[test]
    fn updater_units_are_oneshot_with_a_timer() {
        let (project, state, config) = paths();
        let units = generate(&project, &state, &config, "gpupod", "updater").unwrap();
        let service = &units["gpupod-update.service"];
        assert!(service.contains("Type=oneshot"));
        assert!(!service.contains("Restart=on-failure"));
        assert!(service.contains("TimeoutStartSec=900"));
        assert!(service.contains(
            "helpers/update_agent.py --config \"/srv/gpupod/.provider-state/node.secret.json\""
        ));
        assert!(units["gpupod-update.timer"].contains("OnUnitActiveSec=1h"));
        assert!(units["gpupod-update.timer"].contains("WantedBy=timers.target"));
    }

    #[test]
    fn invalid_users_are_rejected() {
        let (project, state, config) = paths();
        assert!(generate(&project, &state, &config, "BadUser", "provider").is_err());
        assert!(generate(&project, &state, &config, "-lead", "provider").is_err());
        assert!(generate(&project, &state, &config, "9user", "provider").is_err());
        assert!(generate(&project, &state, &config, "gpupod-user", "provider").is_ok());
    }
}
