use std::{
    cmp::Ordering,
    collections::HashMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{args::UpdateArgs, config, i18n::CliLanguage};

pub const DEFAULT_CLI_MANIFEST_URL: &str = "https://www.relaycat.cn/update/cli.json";
pub const FALLBACK_CLI_MANIFEST_URL: &str = "https://relaycat.app/update/cli.json";

/// Hosts allowed to drive a **self-replacement**. A custom `--manifest-url`
/// (a mirror, an internal proxy) may still download and verify, but never
/// overwrites the running executable: otherwise anyone who can point the CLI
/// at their manifest gets code execution on the next update.
const OFFICIAL_MANIFEST_HOSTS: [&str; 2] = ["www.relaycat.cn", "relaycat.app"];

/// Ed25519 public key (base64, 32 bytes) that must sign every update before it
/// is installed. Empty until the release pipeline provisions a key.
///
/// While it is empty, TLS to the official host plus the SHA-256 check are the
/// only anchors, and self-replacement is refused for any non-official
/// The release pipeline must set `RELAYCAT_CLI_UPDATE_PUBLIC_KEY` to the
/// base64 Ed25519 public key.
const CLI_UPDATE_PUBLIC_KEY: &str = match option_env!("RELAYCAT_CLI_UPDATE_PUBLIC_KEY") {
    Some(key) => key,
    None => "",
};

#[derive(Debug, Clone, Deserialize)]
pub struct CliUpdateManifest {
    pub schema: u32,
    pub app: String,
    pub platform: String,
    #[serde(default)]
    pub channel: Option<String>,
    pub latest_version: String,
    #[serde(default)]
    pub min_supported_version: Option<String>,
    #[serde(default)]
    pub mandatory: bool,
    #[serde(default)]
    pub released_at: Option<String>,
    #[serde(default)]
    pub notes: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub downloads: Vec<CliUpdateDownload>,
    #[serde(default)]
    pub homepage_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CliUpdateDownload {
    pub target: String,
    pub url: String,
    pub sha256: String,
    #[serde(default = "default_download_kind")]
    pub kind: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    /// Base64 Ed25519 signature over the raw 32-byte `sha256` digest. Required
    /// once [`CLI_UPDATE_PUBLIC_KEY`] is provisioned: SHA-256 alone proves
    /// nothing, because the expected digest comes from the same manifest.
    #[serde(default)]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadInstallPlan {
    pub can_replace_current_exe: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub latest_version: String,
    pub current_version: String,
    pub mandatory: bool,
}

fn default_download_kind() -> String {
    "binary".to_string()
}

pub fn select_download_for_target<'a>(
    manifest: &'a CliUpdateManifest,
    target: &str,
) -> Option<&'a CliUpdateDownload> {
    manifest
        .downloads
        .iter()
        .find(|download| download.target == target)
}

pub fn compare_versions(left: &str, right: &str) -> Ordering {
    let left_parts = version_parts(left);
    let right_parts = version_parts(right);
    let max_len = left_parts.len().max(right_parts.len());
    for index in 0..max_len {
        match (left_parts.get(index), right_parts.get(index)) {
            (Some(left), Some(right)) => {
                let ordering = left.cmp(right);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(extra), None) => {
                return compare_extra_version_tail(extra, &left_parts[index + 1..], true);
            }
            (None, Some(extra)) => {
                return compare_extra_version_tail(extra, &right_parts[index + 1..], false);
            }
            (None, None) => return Ordering::Equal,
        }
    }
    Ordering::Equal
}

fn compare_extra_version_tail(
    first: &VersionPart,
    rest: &[VersionPart],
    extra_is_left: bool,
) -> Ordering {
    for extra in std::iter::once(first).chain(rest.iter()) {
        if extra.is_zero() {
            continue;
        }
        return match (extra.is_text(), extra_is_left) {
            (true, true) => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Greater,
            (false, false) => Ordering::Less,
        };
    }
    Ordering::Equal
}

impl DownloadInstallPlan {
    pub fn for_target(target: &str, kind: &str) -> Self {
        Self {
            can_replace_current_exe: kind == "binary" && !target.contains("windows"),
        }
    }
}

pub async fn check_notice_for_tui() -> Option<UpdateNotice> {
    let target = current_target_triple();
    let manifest_url = DEFAULT_CLI_MANIFEST_URL.to_string();
    tokio::task::spawn_blocking(move || {
        let primary = fetch_url_to_string_with_timeout(&manifest_url, 2)
            .and_then(|text| notice_from_manifest_text(&text, env!("CARGO_PKG_VERSION"), &target));
        match primary {
            Ok(Some(notice)) => Ok(Some(notice)),
            // A reachable manifest may still lack a usable download for this
            // target (e.g. not yet published on this mirror); try the fallback
            // before concluding there is no update.
            Ok(None) | Err(_) => fetch_url_to_string_with_timeout(FALLBACK_CLI_MANIFEST_URL, 2)
                .and_then(|text| {
                    notice_from_manifest_text(&text, env!("CARGO_PKG_VERSION"), &target)
                })
                .or(primary),
        }
    })
    .await
    .ok()
    .and_then(Result::ok)
    .flatten()
}

pub async fn run_update_command(args: &UpdateArgs, language: CliLanguage) -> Result<()> {
    let manifest_url = args.manifest_url.clone();
    let download_only = args.download_only;
    tokio::task::spawn_blocking(move || {
        run_update_command_blocking(&manifest_url, download_only, language)
    })
    .await
    .context("update task failed")?
}

fn run_update_command_blocking(
    manifest_url: &str,
    download_only: bool,
    language: CliLanguage,
) -> Result<()> {
    println!(
        "{} {}",
        language.t("Checking update manifest:", "正在检查升级清单："),
        manifest_url
    );
    let text = match fetch_url_to_string(manifest_url) {
        Ok(text) => text,
        Err(err) if manifest_url == DEFAULT_CLI_MANIFEST_URL => {
            println!(
                "{} {}",
                language.t(
                    "Primary manifest unreachable, trying:",
                    "主升级清单不可达，改用："
                ),
                FALLBACK_CLI_MANIFEST_URL
            );
            fetch_url_to_string(FALLBACK_CLI_MANIFEST_URL).map_err(|_| err)?
        }
        Err(err) => return Err(err),
    };
    let manifest: CliUpdateManifest =
        serde_json::from_str(&text).context("invalid update manifest")?;
    validate_cli_manifest(&manifest)?;

    let current_version = env!("CARGO_PKG_VERSION");
    if compare_versions(&manifest.latest_version, current_version) != Ordering::Greater {
        println!(
            "{} {}",
            language.t("RelayCat is already up to date:", "RelayCat 已是最新版本："),
            current_version
        );
        return Ok(());
    }

    let target = current_target_triple();
    let download = select_download_for_target(&manifest, &target)
        .with_context(|| format!("no download for target {target}"))?;
    let update_dir = update_download_dir()?;
    fs::create_dir_all(&update_dir)
        .with_context(|| format!("failed to create {}", update_dir.display()))?;
    let file_name = download_file_name(&download.url, &target);
    let output = update_dir.join(file_name);

    println!(
        "{} {} -> {}",
        language.t("Downloading", "正在下载"),
        manifest.latest_version,
        output.display()
    );
    // Check the publisher's signature before spending bandwidth on the
    // download; it pins the digest the payload is verified against below.
    verify_download_signature(download)?;
    download_url_to_path(&download.url, &output)?;
    verify_sha256(&output, &download.sha256)?;
    verify_download_size(&output, download.size_bytes)?;
    println!("{}", language.t("Checksum verified.", "校验通过。"));

    let plan = DownloadInstallPlan::for_target(&download.target, &download.kind);
    // Self-replacement is only allowed from an official manifest: a custom
    // `--manifest-url` (mirror, internal proxy) may download, never overwrite
    // the running executable.
    let trusted_manifest = manifest_is_official(manifest_url);
    if download_only || !plan.can_replace_current_exe || !trusted_manifest {
        println!(
            "{} {}",
            language.t("Downloaded update:", "升级文件已下载："),
            output.display()
        );
        if !trusted_manifest {
            println!(
                "{}",
                language.t(
                    "This manifest is not an official RelayCat host, so the downloaded file was not installed. Install it manually or rerun with the default manifest URL.",
                    "该升级清单不是 RelayCat 官方地址，已下载但未自动替换。请手动安装，或使用默认清单地址重试。",
                )
            );
        } else {
            println!(
                "{}",
                language.t(
                    "Install it manually, or publish a direct non-Windows binary to enable self-replacement.",
                    "请手动安装；非 Windows 直连二进制可启用自动替换。",
                )
            );
        }
        return Ok(());
    }

    replace_current_exe(&output)?;
    println!(
        "{} {}",
        language.t("RelayCat updated to", "RelayCat 已升级到"),
        manifest.latest_version
    );
    Ok(())
}

fn notice_from_manifest_text(
    text: &str,
    current_version: &str,
    target: &str,
) -> Result<Option<UpdateNotice>> {
    let manifest: CliUpdateManifest =
        serde_json::from_str(text).context("invalid update manifest")?;
    validate_cli_manifest(&manifest)?;
    if compare_versions(&manifest.latest_version, current_version) != Ordering::Greater {
        return Ok(None);
    }
    if select_download_for_target(&manifest, target).is_none() {
        return Ok(None);
    }
    let below_minimum = manifest
        .min_supported_version
        .as_deref()
        .map(|min| compare_versions(current_version, min) == Ordering::Less)
        .unwrap_or(false);
    Ok(Some(UpdateNotice {
        latest_version: manifest.latest_version,
        current_version: current_version.to_string(),
        mandatory: manifest.mandatory || below_minimum,
    }))
}

fn validate_cli_manifest(manifest: &CliUpdateManifest) -> Result<()> {
    if manifest.schema != 1 {
        bail!("unsupported update manifest schema {}", manifest.schema);
    }
    if manifest.app != "relaycat" {
        bail!("manifest app must be relaycat");
    }
    if manifest.platform != "cli" {
        bail!("manifest platform must be cli");
    }
    for download in &manifest.downloads {
        validate_update_url(&download.url)?;
    }
    Ok(())
}

fn validate_update_url(url: &str) -> Result<()> {
    let url = url.trim();
    if url.starts_with('-') {
        bail!("update URL must not start with '-'");
    }
    if !url.starts_with("https://") {
        bail!("update URL must use https://: {url}");
    }
    Ok(())
}

fn current_target_triple() -> String {
    let arch = match env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        "x86" => "i686",
        other => other,
    };
    let os = match env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        other => other,
    };
    format!("{arch}-{os}")
}

fn fetch_url_to_string(url: &str) -> Result<String> {
    fetch_url_to_string_with_timeout(url, 30)
}

fn fetch_url_to_string_with_timeout(url: &str, max_time_secs: u64) -> Result<String> {
    validate_update_url(url)?;
    let output = curl_command(url, max_time_secs, Some(Path::new("-")))
        .output()
        .with_context(|| format!("failed to run curl for {url}"))?;
    if !output.status.success() {
        bail!(
            "curl failed for {url}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    String::from_utf8(output.stdout).context("manifest is not valid UTF-8")
}

fn download_url_to_path(url: &str, output: &Path) -> Result<()> {
    validate_update_url(url)?;
    let status = curl_command(url, 300, Some(output))
        .status()
        .with_context(|| format!("failed to run curl for {url}"))?;
    if !status.success() {
        bail!("failed to download {url}");
    }
    Ok(())
}

fn curl_command(url: &str, max_time_secs: u64, output: Option<&Path>) -> Command {
    let mut command = Command::new("curl");
    command
        .arg("--proto")
        .arg("=https")
        .arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg(max_time_secs.to_string());
    if let Some(output) = output {
        command.arg("--output").arg(output);
    }
    command.arg("--").arg(url);
    command
}

fn update_download_dir() -> Result<PathBuf> {
    Ok(config::relaycat_config_dir()?.join("update"))
}

fn download_file_name(url: &str, target: &str) -> String {
    let name = url
        .rsplit('/')
        .next()
        .map(sanitize_download_file_name)
        .unwrap_or_default();
    if name.is_empty() {
        format!("relaycat-{target}")
    } else {
        name
    }
}

fn sanitize_download_file_name(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(|ch| matches!(ch, '.' | '-' | '_'))
        .to_string()
}

fn verify_sha256(path: &Path, expected_hex: &str) -> Result<()> {
    let actual = sha256_file_hex(path)?;
    if !actual.eq_ignore_ascii_case(expected_hex) {
        bail!("checksum mismatch: expected {expected_hex}, got {actual}");
    }
    Ok(())
}

/// Verifies `downloads[].signature` against the pinned key.
///
/// The signature covers the raw 32-byte SHA-256 digest, which is what pins the
/// bytes: a forged manifest cannot re-sign a different binary without the
/// release key, so the digest check and the signature check reinforce each
/// other instead of both coming from the same untrusted document.
fn verify_download_signature(download: &CliUpdateDownload) -> Result<()> {
    verify_download_signature_with_key(CLI_UPDATE_PUBLIC_KEY, download)
}

fn verify_download_signature_with_key(
    public_key_b64: &str,
    download: &CliUpdateDownload,
) -> Result<()> {
    let public_key_b64 = public_key_b64.trim();
    if public_key_b64.is_empty() {
        // No key provisioned in this build; the caller additionally refuses to
        // self-replace unless the manifest came from an official host.
        return Ok(());
    }
    let public_key = base64::engine::general_purpose::STANDARD
        .decode(public_key_b64)
        .context("invalid pinned update key")?;
    let signature = download
        .signature
        .as_deref()
        .context("update download is not signed")?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .context("invalid update signature")?;
    let digest = sha256_digest_bytes(&download.sha256)?;

    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public_key)
        .verify(&digest, &signature)
        .map_err(|_| anyhow::anyhow!("update signature verification failed"))
}

fn sha256_digest_bytes(hex: &str) -> Result<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        bail!("invalid sha256 digest");
    }
    let mut digest = [0_u8; 32];
    for (index, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).context("invalid sha256 digest")?;
        digest[index] = u8::from_str_radix(pair, 16).context("invalid sha256 digest")?;
    }
    Ok(digest)
}

/// Whether a manifest URL may drive self-replacement.
fn manifest_is_official(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    OFFICIAL_MANIFEST_HOSTS.contains(&host.as_str())
}

fn verify_download_size(path: &Path, expected_size: Option<u64>) -> Result<()> {
    let Some(expected_size) = expected_size else {
        return Ok(());
    };
    let actual_size = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    if actual_size != expected_size {
        bail!("download size mismatch: expected {expected_size}, got {actual_size}");
    }
    Ok(())
}

fn sha256_file_hex(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn replace_current_exe(downloaded: &Path) -> Result<()> {
    let current = env::current_exe().context("failed to locate current executable")?;
    let staging = staging_path_for(&current);
    let _ = fs::remove_file(&staging);

    // Stage the bytes next to the destination (same filesystem) and swap them
    // in with a single rename. The old flow renamed the binary away first and
    // then copied the new one into place: a crash in between left no
    // `relaycat` at all, and the copy itself was not atomic.
    fs::copy(downloaded, &staging)
        .with_context(|| format!("failed to stage {}", staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&staging)
            .with_context(|| format!("failed to stat {}", staging.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&staging, permissions)
            .with_context(|| format!("failed to make {} executable", staging.display()))?;
    }
    // Durability: make sure the staged bytes are on disk before the rename
    // publishes them.
    fs::File::open(&staging)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("failed to sync {}", staging.display()))?;

    if let Err(error) = fs::rename(&staging, &current) {
        let _ = fs::remove_file(&staging);
        return Err(error)
            .with_context(|| format!("failed to replace {}", current.display()));
    }
    Ok(())
}

/// `relaycat-0.1.5` → `relaycat-0.1.5.new`.
///
/// Deliberately not `Path::with_extension`: it would treat the trailing
/// version component as the extension and stage `relaycat-0.1.new`, i.e. a
/// different file.
fn staging_path_for(current: &Path) -> PathBuf {
    let mut staged = current.as_os_str().to_os_string();
    staged.push(".new");
    PathBuf::from(staged)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionPart {
    Number(u64),
    Text(String),
}

impl VersionPart {
    fn is_zero(&self) -> bool {
        matches!(self, VersionPart::Number(0))
    }

    fn is_text(&self) -> bool {
        matches!(self, VersionPart::Text(_))
    }
}

impl Ord for VersionPart {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (VersionPart::Number(left), VersionPart::Number(right)) => left.cmp(right),
            (VersionPart::Text(left), VersionPart::Text(right)) => left.cmp(right),
            (VersionPart::Number(_), VersionPart::Text(_)) => Ordering::Greater,
            (VersionPart::Text(_), VersionPart::Number(_)) => Ordering::Less,
        }
    }
}

impl PartialOrd for VersionPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn version_parts(version: &str) -> Vec<VersionPart> {
    version
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u64>()
                .map(VersionPart::Number)
                .unwrap_or_else(|_| VersionPart::Text(part.to_ascii_lowercase()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_file_name_rejects_path_traversal_segments() {
        assert_eq!(
            download_file_name("https://example.com/releases/..", "aarch64-apple-darwin"),
            "relaycat-aarch64-apple-darwin"
        );
        assert_eq!(
            download_file_name(
                "https://example.com/releases/relaycat\\evil",
                "aarch64-apple-darwin",
            ),
            "relaycat-evil"
        );
    }

    #[test]
    fn verify_download_size_rejects_manifest_mismatch() {
        let dir = tempfile_dir("relaycat-update-size");
        let path = dir.join("relaycat");
        fs::write(&path, b"abc").expect("write update");

        assert!(verify_download_size(&path, Some(3)).is_ok());
        let err = verify_download_size(&path, Some(4)).expect_err("size mismatch");
        assert!(err.to_string().contains("download size mismatch"));
        assert!(verify_download_size(&path, None).is_ok());
    }

    #[test]
    fn validate_update_url_requires_https_and_rejects_options() {
        assert!(validate_update_url("https://www.relaycat.cn/update/cli.json").is_ok());
        assert!(validate_update_url("http://www.relaycat.cn/update/cli.json").is_err());
        assert!(validate_update_url("file:///etc/passwd").is_err());
        assert!(validate_update_url("-K/tmp/config").is_err());
    }

    #[test]
    fn curl_command_places_end_of_options_before_url() {
        let cmd = curl_command("https://example.com/cli.json", 10, Some(Path::new("-")));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let dash_pos = args.iter().position(|a| a == "--").expect("has --");
        assert_eq!(args.get(dash_pos + 1).map(String::as_str), Some("https://example.com/cli.json"));
        assert!(args.windows(2).any(|w| w == ["--output", "-"]));
    }

    fn tempfile_dir(prefix: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "{}-{}-{}",
            prefix,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn a_pinned_key_rejects_unsigned_or_tampered_downloads() {
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public =
            base64::engine::general_purpose::STANDARD.encode(key_pair.public_key().as_ref());
        let digest = [0x11_u8; 32];
        let signature = base64::engine::general_purpose::STANDARD
            .encode(key_pair.sign(&digest).as_ref());

        let mut download = CliUpdateDownload {
            target: "aarch64-apple-darwin".into(),
            url: "https://example.com/relaycat".into(),
            sha256: "11".repeat(32),
            kind: "binary".into(),
            size_bytes: None,
            signature: Some(signature),
        };
        assert!(verify_download_signature_with_key(&public, &download).is_ok());

        // Same digest value, signature does not match it.
        download.sha256 = "22".repeat(32);
        assert!(verify_download_signature_with_key(&public, &download).is_err());

        // Missing signature is a hard failure once a key is pinned.
        download.sha256 = "11".repeat(32);
        download.signature = None;
        assert!(verify_download_signature_with_key(&public, &download).is_err());

        // An unprovisioned key is a no-op; the official-host check still gates
        // self-replacement.
        assert!(verify_download_signature_with_key("", &download).is_ok());
    }

    #[test]
    fn only_official_https_manifests_may_self_replace() {
        assert!(manifest_is_official("https://www.relaycat.cn/update/cli.json"));
        assert!(manifest_is_official("https://relaycat.app/update/cli.json"));
        assert!(!manifest_is_official("http://www.relaycat.cn/update/cli.json"));
        assert!(!manifest_is_official("https://evil.example/update/cli.json"));
        assert!(!manifest_is_official(
            "https://www.relaycat.cn.evil.example/update/cli.json"
        ));
    }

    #[test]
    fn the_staging_path_keeps_versioned_binary_names_intact() {
        assert_eq!(
            staging_path_for(Path::new("/usr/local/bin/relaycat-0.1.5")),
            PathBuf::from("/usr/local/bin/relaycat-0.1.5.new")
        );
        assert_eq!(
            staging_path_for(Path::new("/usr/local/bin/relaycat")),
            PathBuf::from("/usr/local/bin/relaycat.new")
        );
    }
}
