//! 記録装置と中央間の測定（Python 版 benchmark.py の移植）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use gpupod_common::AgentError;
use rand::RngCore;

use crate::support::benchmarks::{save_result, MIB, MAX_NETWORK_BYTES};
use crate::support::config::ProviderConfig;
use crate::support::files::HeldLock;
use crate::support::telemetry::disk_usage;

/// fio で 1MiB QD1 direct I/O の順次読み書きを測る。
pub fn measure_disk(directory: &Path, size_mib: i64, fio: &str) -> Result<serde_json::Value> {
    if !(16..=4096).contains(&size_mib) {
        bail!(AgentError::new("Disk sample must be between 16 and 4096 MiB"));
    }
    let directory = std::fs::canonicalize(directory).context("Cannot access the measurement directory")?;
    if !directory.is_dir() {
        bail!(AgentError::new("The measurement directory needs the sample size plus 256 MiB free"));
    }
    let (_, free) = disk_usage(&directory).map_err(|_| {
        AgentError::new("The measurement directory needs the sample size plus 256 MiB free")
    })?;
    if free < (size_mib as u64 + 256) * MIB as u64 {
        bail!(AgentError::new(
            "The measurement directory needs the sample size plus 256 MiB free"
        ));
    }
    let executable = which(fio).ok_or_else(|| {
        AgentError::new("Install the optional fio system package to measure disk throughput")
    })?;
    let executable = std::fs::canonicalize(&executable).unwrap_or(executable);
    let version = Command::new(&executable)
        .arg("--version")
        .output()
        .context("Cannot run fio")?;
    let version = String::from_utf8_lossy(&version.stdout).trim().to_string();
    if !valid_fio_version(&version) {
        bail!(AgentError::new("The selected command is not the fio 3.x I/O benchmark"));
    }
    let mut rates: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    // 既存ファイル・ブロックデバイス・全体キャッシュを使わない。
    let temporary = tempfile::Builder::new()
        .prefix(".gpupod-benchmark-")
        .tempdir_in(&directory)
        .context("Cannot create a temporary benchmark directory")?;
    for operation in ["write", "read"] {
        let output = Command::new(&executable)
            .args([
                "--name=gpupod-sequential",
                "--filename=sample",
                &format!("--size={size_mib}M"),
                &format!("--rw={operation}"),
                "--bs=1M",
                "--ioengine=sync",
                "--iodepth=1",
                "--numjobs=1",
                "--direct=1",
                "--end_fsync=1",
                "--fallocate=none",
                "--refill_buffers=1",
                "--randrepeat=0",
                "--eta=never",
                "--output-format=json",
                "--runtime=60",
            ])
            .current_dir(temporary.path())
            .output()
            .context("Cannot run fio")?;
        let parsed: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| AgentError::new("Disk measurement failed; check fio and direct-I/O filesystem support"))?;
        let jobs = parsed
            .get("jobs")
            .and_then(|v| v.as_array())
            .filter(|jobs| jobs.len() == 1)
            .ok_or_else(|| AgentError::new("fio did not complete the entire sample; previous result retained"))?;
        let job = &jobs[0];
        let stats = job
            .get(operation)
            .ok_or_else(|| AgentError::new("fio did not complete the entire sample; previous result retained"))?;
        let failed = job.get("error").and_then(|v| v.as_i64()).unwrap_or(1) != 0;
        let io_bytes = stats.get("io_bytes").and_then(|v| v.as_i64()).unwrap_or(0);
        if failed || io_bytes != size_mib * MIB as i64 {
            bail!(AgentError::new(
                "fio did not complete the entire sample; previous result retained"
            ));
        }
        let bw = stats
            .get("bw_bytes")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| AgentError::new("Disk measurement failed; check fio and direct-I/O filesystem support"))?;
        rates.insert(operation.to_string(), serde_json::json!(bw / MIB as f64));
    }
    Ok(serde_json::json!({
        "measured_at": crate::support::benchmarks::timestamp(),
        "size_bytes": size_mib * MIB as i64,
        "read_mib_s": rates["read"],
        "write_mib_s": rates["write"],
        "method": "fio-sequential-1MiB-qd1",
        "direct_io": true,
    }))
}

/// Python 版の fullmatch と同じ判定。
pub fn valid_fio_version(version: &str) -> bool {
    let rest = match version.strip_prefix("fio-3.") {
        Some(rest) => rest,
        None => return false,
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return false;
    }
    match &rest[digits.len()..] {
        "" => true,
        suffix => suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.')),
    }
}

/// 中央サーバと往復して上下転送速度を測る。
pub fn measure_network(config: &ProviderConfig, size_mib: i64) -> Result<serde_json::Value> {
    if !(1..=(MAX_NETWORK_BYTES / MIB)).contains(&size_mib) {
        bail!(AgentError::new(
            "Network sample must be between 1 and 64 MiB in each direction"
        ));
    }
    let size = size_mib * MIB as i64;
    let url = format!("{}/api/nodes/{}/benchmark/", config.url, config.node_id);
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .context("Cannot create the HTTP client")?;
    let bearer = format!("Bearer {}", config.token);
    let download_seconds = download_sample(&client, &url, &bearer, size)?;
    let upload_seconds = upload_sample(&client, &url, &bearer, size)?;
    Ok(serde_json::json!({
        "measured_at": crate::support::benchmarks::timestamp(),
        "size_bytes": size,
        "scope": "central",
        "download_mbps": size as f64 * 8.0 / download_seconds.max(1e-9) / 1e6,
        "upload_mbps": size as f64 * 8.0 / upload_seconds.max(1e-9) / 1e6,
    }))
}

fn download_sample(
    client: &reqwest::blocking::Client,
    url: &str,
    bearer: &str,
    size: i64,
) -> Result<f64> {
    let started = Instant::now();
    let request_url = format!("{url}?size={size}");
    let mut response = match client
        .get(&request_url)
        .header(reqwest::header::AUTHORIZATION, bearer)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .timeout(Duration::from_secs(15))
        .send()
    {
        Ok(response) => response,
        Err(_) => bail!(AgentError::new(
            "Central download measurement refused (check credentials, cooldown and proxy limits)"
        )),
    };
    let encoding = response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("identity")
        .to_string();
    let length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    if response.status().as_u16() != 200 || encoding != "identity" || length != size.to_string() {
        bail!(AgentError::new(
            "Central download measurement refused (check credentials, cooldown and proxy limits)"
        ));
    }
    let mut received: i64 = 0;
    let mut buffer = vec![0u8; 65536];
    loop {
        if started.elapsed() > Duration::from_secs(60) {
            bail!(AgentError::new("Download exceeded its byte/time limit"));
        }
        let read = response.read(&mut buffer).context("Cannot read the download sample")?;
        if read == 0 {
            break;
        }
        received += read as i64;
        if received > size {
            bail!(AgentError::new("Download exceeded its byte/time limit"));
        }
    }
    if received != size {
        bail!(AgentError::new("Incomplete download sample"));
    }
    Ok(started.elapsed().as_secs_f64())
}

fn upload_sample(
    client: &reqwest::blocking::Client,
    url: &str,
    bearer: &str,
    size: i64,
) -> Result<f64> {
    let started = Instant::now();
    let body = reqwest::blocking::Body::sized(UploadSample::new(size as usize), size as u64);
    let response = client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, bearer)
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header(reqwest::header::CONTENT_LENGTH, size.to_string())
        .timeout(Duration::from_secs(15))
        .body(body)
        .send();
    let response = match response {
        Ok(response) => response,
        Err(_) => bail!(AgentError::new(
            "Network measurement failed; previous result retained"
        )),
    };
    if response.status().as_u16() != 200 {
        bail!(AgentError::new(
            "Central upload measurement refused (check cooldown and proxy body limits)"
        ));
    }
    let payload: serde_json::Value = response.json().context("Cannot read the upload response")?;
    if payload.get("received_bytes").and_then(|v| v.as_i64()) != Some(size) {
        bail!(AgentError::new(
            "Central upload measurement refused (check cooldown and proxy body limits)"
        ));
    }
    let elapsed = started.elapsed().as_secs_f64();
    if elapsed > 60.0 {
        bail!(AgentError::new("Upload exceeded its time limit"));
    }
    Ok(elapsed)
}

/// 同一の乱数ブロックを繰り返すアップロード本文。
pub struct UploadSample {
    remaining: usize,
    block: Vec<u8>,
    offset: usize,
}

impl UploadSample {
    pub fn new(size: usize) -> UploadSample {
        let mut block = vec![0u8; 65536];
        rand::thread_rng().fill_bytes(&mut block);
        UploadSample { remaining: size, block, offset: 0 }
    }
}

impl Read for UploadSample {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let available = self.block.len() - self.offset;
        let take = available.min(self.remaining).min(buffer.len());
        buffer[..take].copy_from_slice(&self.block[self.offset..self.offset + take]);
        self.offset = (self.offset + take) % self.block.len();
        self.remaining -= take;
        Ok(take)
    }
}

fn which(command: &str) -> Option<PathBuf> {
    if command.contains('/') {
        let path = PathBuf::from(command);
        return path.is_file().then_some(path);
    }
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(command);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// CLI エントリ。測定結果を保存して JSON を出力する。
pub fn main(options: &Options) -> Result<()> {
    if options.disk_path.is_none() && !options.network {
        bail!("Select --disk-path and/or --network");
    }
    if options.network && options.config.is_none() {
        bail!("--network requires --config");
    }
    let state_dir = &options.state_dir;
    std::fs::create_dir_all(state_dir)?;
    crate::support::files::fs_set_mode(state_dir, 0o700)?;
    let _lock = HeldLock::try_exclusive(&state_dir.join("benchmark.lock"))?
        .ok_or_else(|| AgentError::new("Another provider operation is still running"))?;
    let mut results = serde_json::Map::new();
    if let Some(path) = &options.disk_path {
        let measurement = measure_disk(path, options.disk_size_mib, &options.fio)?;
        save_result(state_dir, "disk", &measurement)?;
        results.insert("disk".to_string(), measurement);
    }
    if options.network {
        let config = crate::support::config::load_provider_config(options.config.as_ref().unwrap())?;
        let measurement = measure_network(&config, options.network_size_mib)?;
        save_result(state_dir, "network", &measurement)?;
        results.insert("network".to_string(), measurement);
    }
    println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(results))?);
    Ok(())
}

#[derive(Debug, Default)]
pub struct Options {
    pub state_dir: PathBuf,
    pub config: Option<PathBuf>,
    pub disk_path: Option<PathBuf>,
    pub disk_size_mib: i64,
    pub fio: String,
    pub network: bool,
    pub network_size_mib: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fio_version_matches_the_python_pattern() {
        assert!(valid_fio_version("fio-3.36"));
        assert!(valid_fio_version("fio-3.39-1"));
        assert!(valid_fio_version("fio-3.0.21"));
        assert!(!valid_fio_version("fio-2.36"));
        assert!(!valid_fio_version("fio-3."));
        assert!(!valid_fio_version("fio-3x"));
        assert!(!valid_fio_version("other-3.36"));
    }

    #[test]
    fn disk_sample_size_is_bounded() {
        let error = measure_disk(Path::new("/tmp"), 8, "fio").unwrap_err().to_string();
        assert!(error.contains("between 16 and 4096 MiB"), "{error}");
        let error = measure_disk(Path::new("/tmp"), 8192, "fio").unwrap_err().to_string();
        assert!(error.contains("between 16 and 4096 MiB"), "{error}");
    }

    #[test]
    fn missing_fio_is_reported_clearly() {
        // /tmp は十分空いている前提。fio が無い環境では専用メッセージになる。
        if which("fio").is_some() {
            return;
        }
        let error = measure_disk(Path::new("/tmp"), 16, "definitely-not-fio")
            .unwrap_err()
            .to_string();
        assert!(error.contains("fio"), "{error}");
    }

    #[test]
    fn network_sample_size_is_bounded() {
        let config = ProviderConfig {
            url: "https://central.example.org".into(),
            node_id: "node-1".into(),
            token: "token".into(),
            relay_host: "relay.example.org".into(),
            relay_host_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFakeFakeFakeFakeFakeFakeFakeFake000000".into(),
            direct_host: None,
            direct_bind: None,
            limits: Default::default(),
            min_disk_free_bytes: 0,
            runtime: None,
            watchdog_timeout: 180.0,
        };
        let error = measure_network(&config, 0).unwrap_err().to_string();
        assert!(error.contains("between 1 and 64 MiB"), "{error}");
        let error = measure_network(&config, 128).unwrap_err().to_string();
        assert!(error.contains("between 1 and 64 MiB"), "{error}");
    }

    #[test]
    fn upload_sample_yields_repeated_blocks() {
        let mut sample = UploadSample::new(100_000);
        let mut total = 0;
        let mut buffer = [0u8; 4096];
        loop {
            let read = sample.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            total += read;
        }
        assert_eq!(total, 100_000);
    }

    #[test]
    fn main_requires_a_target() {
        let options = Options {
            state_dir: PathBuf::from("/tmp"),
            disk_size_mib: 256,
            fio: "fio".into(),
            network_size_mib: 16,
            ..Default::default()
        };
        assert!(main(&options).is_err());
        let options = Options { network: true, ..options };
        assert!(main(&options).is_err());
    }
}
