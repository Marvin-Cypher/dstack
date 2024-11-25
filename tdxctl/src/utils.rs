use std::{
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio},
};

use anyhow::{bail, Context, Result};
use fs_err as fs;
use serde::{de::DeserializeOwned, Deserialize};
use serde_human_bytes as hex_bytes;
use sha2::{digest::Output, Digest};
use tdx_attest as att;

/// This code is not defined in the TCG specification.
/// See https://trustedcomputinggroup.org/wp-content/uploads/PC-ClientSpecific_Platform_Profile_for_TPM_2p0_Systems_v51.pdf
const DSTACK_EVENT_TAG: u32 = 0x08000001;


pub fn deserialize_json_file<T: DeserializeOwned>(path: impl AsRef<Path>) -> Result<T> {
    let data = fs::read_to_string(path).context("Failed to read file")?;
    serde_json::from_str(&data).context("Failed to parse json")
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut sha256 = sha2::Sha256::new();
    sha256.update(data);
    sha256.finalize().into()
}

pub fn sha256_file(path: impl AsRef<Path>) -> Result<[u8; 32]> {
    let data = fs::read(path).context("Failed to read file")?;
    Ok(sha256(&data))
}

pub fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(src.as_ref())? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
        } else {
            fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub struct HashingFile<H, F> {
    file: F,
    hasher: H,
}

impl<H: Digest, F: Read> Read for HashingFile<H, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bytes_read = self.file.read(buf)?;
        self.hasher.update(&buf[..bytes_read]);
        Ok(bytes_read)
    }
}

impl<H: Digest, F> HashingFile<H, F> {
    pub fn new(file: F) -> Self {
        Self {
            file,
            hasher: H::new(),
        }
    }

    pub fn finalize(self) -> Output<H> {
        self.hasher.finalize()
    }
}

pub fn extend_rtmr3(event: &str, payload: &[u8]) -> Result<()> {
    extend_rtmr(3, DSTACK_EVENT_TAG, event, payload)
}

pub fn extend_rtmr(index: u32, event_type: u32, event: &str, payload: &[u8]) -> Result<()> {
    let log =
        att::eventlog::TdxEventLog::new(index, event_type, event.to_string(), payload.to_vec());
    att::extend_rtmr(index, event_type, log.digest).context("Failed to extend RTMR")?;
    let hexed_payload = hex::encode(payload);
    let hexed_digest = hex_fmt::HexFmt(&log.digest);
    println!("Extended RTMR{index}: event={event}, payload={hexed_payload}, digest={hexed_digest}");
    att::log_rtmr_event(&log).context("Failed to log RTMR extending event")?;
    Ok(())
}

pub fn run_command_with_stdin(
    command: &str,
    args: &[&str],
    stdin: impl AsRef<[u8]>,
) -> Result<Vec<u8>> {
    let mut child = Command::new("/usr/bin/env")
        .args(&[command])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(format!("Failed to run {}", command))?;
    let mut child_stdin = child.stdin.take().context("Failed to get stdin")?;
    child_stdin
        .write_all(stdin.as_ref())
        .context("Failed to write to stdin")?;
    drop(child_stdin);
    let output = child
        .wait_with_output()
        .context(format!("Failed to wait for {}", command))?;
    if !output.status.success() {
        bail!(
            "Command {} failed: {}",
            command,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

pub fn run_command(command: &str, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("/usr/bin/env")
        .arg(command)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context(format!("Failed to run {}", command))?;
    if !output.status.success() {
        bail!(
            "Command {} failed: {}",
            command,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
}

#[derive(Deserialize)]
pub struct AppCompose {
    pub features: Vec<String>,
    pub runner: String,
    pub docker_compose_file: Option<String>,
}

impl AppCompose {
    pub fn feature_enabled(&self, feature: &str) -> bool {
        self.features.contains(&feature.to_string())
    }
}

#[derive(Deserialize)]
pub struct VmConfig {
    #[serde(with = "hex_bytes")]
    pub rootfs_hash: Vec<u8>,
    pub kms_url: Option<String>,
    pub tproxy_url: Option<String>,
}

#[derive(Deserialize)]
pub struct AppKeys {
    pub app_key: String,
    pub disk_crypt_key: String,
    #[serde(with = "hex_bytes", default)]
    pub env_crypt_key: Vec<u8>,
    pub certificate_chain: Vec<String>,
}
