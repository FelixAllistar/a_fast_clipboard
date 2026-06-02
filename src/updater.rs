use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const OWNER: &str = "FelixAllistar";
const REPO: &str = "a_fast_clipboard";
const USER_AGENT: &str = concat!("a_fast_clipboard/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug)]
pub struct UpdateInfo {
    pub version: String,
    pub asset_name: String,
    pub download_url: String,
    pub digest: Option<String>,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

pub fn latest_update() -> Result<Option<UpdateInfo>> {
    let url = format!("https://api.github.com/repos/{OWNER}/{REPO}/releases/latest");
    let release = reqwest::blocking::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .context("failed to query GitHub release")?
        .error_for_status()
        .context("GitHub release query failed")?
        .json::<GitHubRelease>()
        .context("failed to parse GitHub release")?;

    let latest_version = parse_version(&release.tag_name)?;
    let current_version = parse_version(env!("CARGO_PKG_VERSION"))?;
    if latest_version <= current_version {
        return Ok(None);
    }

    let expected_asset_name = format!("a_fast_clipboard-v{}-windows-x64.exe", latest_version);
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == expected_asset_name)
        .ok_or_else(|| anyhow!("latest release has no Windows exe asset"))?;

    Ok(Some(UpdateInfo {
        version: release.tag_name.trim_start_matches('v').to_string(),
        asset_name: asset.name,
        download_url: asset.browser_download_url,
        digest: asset.digest,
    }))
}

pub fn stage_and_launch_update(info: &UpdateInfo) -> Result<()> {
    if cfg!(debug_assertions) {
        bail!("updates install only from release builds");
    }

    let current_exe = std::env::current_exe().context("failed to locate current exe")?;
    let update_dir = std::env::temp_dir().join("AFastClipboard").join("update");
    fs::create_dir_all(&update_dir).context("failed to create update directory")?;

    let old_pid = std::process::id();
    let token = update_token(old_pid);
    let staged_exe = update_dir.join(&info.asset_name);
    download_asset(&info.download_url, info.digest.as_deref(), &staged_exe)?;

    let helper_exe = update_dir.join("a_fast_clipboard_update_helper.exe");
    fs::copy(&current_exe, &helper_exe).context("failed to stage update helper")?;

    Command::new(helper_exe)
        .arg("--apply-update")
        .arg(&staged_exe)
        .arg(&current_exe)
        .arg(old_pid.to_string())
        .arg(token)
        .spawn()
        .context("failed to launch update helper")?;

    Ok(())
}

pub fn apply_update_from_args(args: &[String]) -> Result<bool> {
    if args.first().map(String::as_str) != Some("--apply-update") {
        return Ok(false);
    }

    let staged_exe = args
        .get(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing staged update path"))?;
    let target_exe = args
        .get(2)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing target exe path"))?;
    let old_pid = args
        .get(3)
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| anyhow!("missing old process id"))?;
    let token = args.get(4).ok_or_else(|| anyhow!("missing update token"))?;

    validate_helper_request(&staged_exe, &target_exe, old_pid, token)?;

    wait_for_process_exit(old_pid);
    replace_when_ready(&staged_exe, &target_exe)?;
    Command::new(&target_exe)
        .spawn()
        .context("failed to restart updated app")?;
    Ok(true)
}

fn download_asset(url: &str, digest: Option<&str>, destination: &Path) -> Result<()> {
    let bytes = reqwest::blocking::Client::new()
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .context("failed to download update")?
        .error_for_status()
        .context("update download failed")?
        .bytes()
        .context("failed to read update download")?;

    if let Some(expected_digest) = digest {
        verify_digest(&bytes, expected_digest)?;
    }

    let temp_destination = destination.with_extension(format!("download-{}", std::process::id()));
    fs::write(&temp_destination, bytes).context("failed to write staged update")?;
    if destination.exists() {
        fs::remove_file(destination).context("failed to remove previous staged update")?;
    }
    fs::rename(&temp_destination, destination).context("failed to finalize staged update")?;
    Ok(())
}

fn replace_when_ready(staged_exe: &Path, target_exe: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match fs::copy(staged_exe, target_exe) {
            Ok(_) => return Ok(()),
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => return Err(error).context("failed to replace app executable"),
        }
    }
}

fn wait_for_process_exit(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while process_is_running(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(200));
    }
}

fn process_is_running(pid: u32) -> bool {
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .any(|line| line.contains(&pid.to_string()))
            })
            .unwrap_or(false)
    }

    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

fn parse_version(value: &str) -> Result<Version> {
    Version::parse(value.trim().trim_start_matches('v')).context("invalid release version")
}

fn verify_digest(bytes: &[u8], expected_digest: &str) -> Result<()> {
    let expected = expected_digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("unsupported release digest format"))?;
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!("downloaded update digest did not match release metadata");
    }
    Ok(())
}

fn update_token(old_pid: u32) -> String {
    let material = format!(
        "a_fast_clipboard:update:{old_pid}:{}",
        env!("CARGO_PKG_VERSION")
    );
    format!("{:x}", Sha256::digest(material.as_bytes()))
}

fn validate_helper_request(
    staged_exe: &Path,
    target_exe: &Path,
    old_pid: u32,
    token: &str,
) -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to locate helper exe")?;
    let expected_dir = std::env::temp_dir().join("AFastClipboard").join("update");
    let staged_parent = staged_exe
        .parent()
        .ok_or_else(|| anyhow!("staged update has no parent directory"))?;
    let helper_parent = current_exe
        .parent()
        .ok_or_else(|| anyhow!("helper has no parent directory"))?;

    if helper_parent != expected_dir || staged_parent != expected_dir {
        bail!("update helper paths are invalid");
    }
    if !staged_exe
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with("a_fast_clipboard-v") && name.ends_with("-windows-x64.exe"))
        .unwrap_or(false)
    {
        bail!("staged update asset name is invalid");
    }
    if target_exe.file_name() != Some(std::ffi::OsStr::new("a_fast_clipboard.exe")) {
        bail!("target exe name is invalid");
    }
    if token != update_token(old_pid) {
        bail!("update helper token is invalid");
    }
    Ok(())
}
