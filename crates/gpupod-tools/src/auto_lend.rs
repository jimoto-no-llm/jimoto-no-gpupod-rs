//! GPU 活動検出（Python 版 auto_lend.py の移植）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use gpupod_common::load_templates;

use crate::support::agentlite::Agent;
use crate::support::devices;
use crate::support::docker::DockerEngine;
use crate::support::telemetry::IdlePolicy;

#[derive(Debug, Default)]
pub struct Options {
    pub state_dir: PathBuf,
    pub templates: PathBuf,
    pub devices: Option<PathBuf>,
    pub docker_socket: PathBuf,
    pub idle_seconds: f64,
    pub interval: f64,
    pub mode: String,
    pub signal_file: Option<PathBuf>,
    pub once: bool,
}

/// 1 回分の判定を返す（テスト用）。
pub fn iteration(
    agent: &Agent,
    policy: &mut IdlePolicy,
    signal_file: Option<&PathBuf>,
    now: f64,
) -> Result<(serde_json::Value, bool)> {
    let activity = match signal_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|exc| anyhow::anyhow!("Cannot read the signal file: {exc}"))?;
            match text.trim() {
                "busy" => Some(true),
                "idle" => Some(false),
                _ => None,
            }
        }
        None => agent.external_activity(),
    };
    let busy = policy.update(activity, now);
    Ok((serde_json::json!({"external_activity": activity, "auto_busy": busy}), busy))
}

/// 対話なしで繰り返す本体。
pub fn run(options: &Options) -> Result<i32> {
    if options.interval < 0.1 || options.idle_seconds < 0.0 {
        bail!("interval must be >=0.1 and idle-seconds >=0");
    }
    if !matches!(options.mode.as_str(), "drain" | "stop") {
        bail!("mode must be drain or stop");
    }
    let templates = load_templates(&options.templates)?;
    let gpus = if options.docker_socket.is_file() || options.devices.is_some() {
        devices::load_gpus(options.devices.as_deref())?
    } else {
        devices::load_gpus(options.devices.as_deref())?
    };
    let mut agent = Agent::new(
        &options.state_dir,
        DockerEngine::new(&options.docker_socket),
        templates,
        gpus,
    )?;
    let mut policy = IdlePolicy::new(options.idle_seconds);
    let start = Instant::now();
    loop {
        let (payload, busy) = iteration(&agent, &mut policy, options.signal_file.as_ref(), start.elapsed().as_secs_f64())?;
        let snapshot = agent.status()?;
        if snapshot.auto_busy != busy {
            agent.set_auto_busy(busy, &options.mode)?;
        }
        println!("{}", serde_json::to_string(&payload)?);
        if options.once {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_secs_f64(options.interval));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_bounds_are_enforced() {
        let options = Options {
            state_dir: PathBuf::from("/tmp"),
            templates: PathBuf::from("/tmp/templates.json"),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            idle_seconds: 60.0,
            interval: 0.01,
            mode: "drain".into(),
            once: true,
            ..Default::default()
        };
        assert!(run(&options).is_err());
    }

    #[test]
    fn mode_is_validated() {
        let options = Options {
            state_dir: PathBuf::from("/tmp"),
            templates: PathBuf::from("/tmp/templates.json"),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            idle_seconds: 60.0,
            interval: 1.0,
            mode: "bogus".into(),
            once: true,
            ..Default::default()
        };
        assert!(run(&options).is_err());
    }
}
