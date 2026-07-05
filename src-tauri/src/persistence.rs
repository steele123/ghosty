use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};

use crate::models::{PresenceStatus, StartupStatus};

pub fn data_dir() -> Result<PathBuf> {
    let mut dir = dirs::data_dir().context("Unable to find the application data directory")?;
    dir.push("Ghosty");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn read_startup_status() -> StartupStatus {
    read_string("startupStatus")
        .as_deref()
        .map(parse_startup_status)
        .unwrap_or(StartupStatus::Last)
}

pub fn write_startup_status(status: StartupStatus) -> Result<()> {
    write_string("startupStatus", startup_status_text(status))
}

pub fn read_session_status() -> PresenceStatus {
    read_string("status")
        .as_deref()
        .map(parse_presence_status)
        .unwrap_or(PresenceStatus::Offline)
}

pub fn write_session_status(status: PresenceStatus) -> Result<()> {
    write_string("status", status.as_xmpp())
}

pub fn read_helper_friend() -> bool {
    read_string("helperFriend")
        .as_deref()
        .map(parse_bool)
        .unwrap_or(true)
}

pub fn write_helper_friend(enabled: bool) -> Result<()> {
    write_string("helperFriend", if enabled { "true" } else { "false" })
}

pub fn read_auto_accept() -> bool {
    read_string("autoAccept")
        .as_deref()
        .map(parse_bool)
        .unwrap_or(false)
}

pub fn write_auto_accept(enabled: bool) -> Result<()> {
    write_string("autoAccept", if enabled { "true" } else { "false" })
}

pub fn read_auto_accept_delay_ms() -> u32 {
    read_string("autoAcceptDelayMs")
        .as_deref()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .map(clamp_auto_accept_delay)
        .unwrap_or(2_000)
}

pub fn write_auto_accept_delay_ms(delay_ms: u32) -> Result<()> {
    write_string(
        "autoAcceptDelayMs",
        &clamp_auto_accept_delay(delay_ms).to_string(),
    )
}

pub fn read_discord_webhook_url() -> String {
    read_string("discordWebhookUrl")
        .map(|url| url.trim().to_string())
        .unwrap_or_default()
}

pub fn write_discord_webhook_url(url: &str) -> Result<()> {
    write_string("discordWebhookUrl", url.trim())
}

pub fn read_certificate() -> Option<Vec<u8>> {
    let mut path = data_dir().ok()?;
    path.push("localhostCert.pfx");
    fs::read(path).ok()
}

pub fn write_certificate(bytes: &[u8]) -> Result<()> {
    let mut path = data_dir()?;
    path.push("localhostCert.pfx");
    atomic_write(&path, bytes)
}

fn read_string(file: &str) -> Option<String> {
    let mut path = data_dir().ok()?;
    path.push(file);
    fs::read_to_string(path).ok()
}

fn write_string(file: &str, value: &str) -> Result<()> {
    let mut path = data_dir()?;
    path.push(file);
    atomic_write(&path, value.as_bytes())?;
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = temp_path_for(path);

    fs::write(&temp, bytes)?;
    if cfg!(windows) {
        replace_file_on_windows(&temp, path)?;
    } else {
        fs::rename(&temp, path)?;
    }
    Ok(())
}

fn replace_file_on_windows(temp: &Path, path: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..8 {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) if is_transient_replace_error(&error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(5 * (attempt + 1)));
                continue;
            }
            Err(error) => return Err(error.into()),
        }

        match fs::rename(temp, path) {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(error.kind(), ErrorKind::AlreadyExists)
                    || is_transient_replace_error(&error) =>
            {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(5 * (attempt + 1)));
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("Unable to replace {}", path.display())))
}

fn is_transient_replace_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::Interrupted
    )
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ghosty");
    let unique = format!(
        "{file_name}.{}.{}.tmp",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    path.with_file_name(unique)
}

fn parse_presence_status(value: &str) -> PresenceStatus {
    match value.trim().to_ascii_lowercase().as_str() {
        "chat" => PresenceStatus::Chat,
        "mobile" => PresenceStatus::Mobile,
        _ => PresenceStatus::Offline,
    }
}

fn parse_startup_status(value: &str) -> StartupStatus {
    match value.trim().to_ascii_lowercase().as_str() {
        "chat" => StartupStatus::Chat,
        "offline" => StartupStatus::Offline,
        "mobile" => StartupStatus::Mobile,
        _ => StartupStatus::Last,
    }
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

fn clamp_auto_accept_delay(delay_ms: u32) -> u32 {
    delay_ms.clamp(0, 10_000)
}

fn startup_status_text(status: StartupStatus) -> &'static str {
    match status {
        StartupStatus::Chat => "chat",
        StartupStatus::Offline => "offline",
        StartupStatus::Mobile => "mobile",
        StartupStatus::Last => "last",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "ghosty-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        path
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let path = temp_file("atomic-replace");
        fs::write(&path, b"old").expect("seed file should write");

        atomic_write(&path, b"new").expect("atomic write should succeed");

        assert_eq!(fs::read(&path).expect("file should read"), b"new");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn atomic_write_uses_sibling_tmp_file_for_extensionless_paths() {
        let path = temp_file("atomic-extensionless");

        atomic_write(&path, b"value").expect("atomic write should succeed");

        assert_eq!(fs::read(&path).expect("file should read"), b"value");
        assert_no_temp_files_for(&path);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn atomic_write_uses_sibling_tmp_file_with_existing_extension() {
        let mut path = temp_file("atomic-extension");
        path.set_extension("pfx");

        atomic_write(&path, b"cert").expect("atomic write should succeed");

        assert_eq!(fs::read(&path).expect("file should read"), b"cert");
        assert_no_temp_files_for(&path);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn temp_path_for_uses_unique_sibling_paths() {
        let path = temp_file("atomic-unique");

        let first = temp_path_for(&path);
        let second = temp_path_for(&path);

        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        assert_eq!(second.parent(), path.parent());
    }

    #[test]
    fn parse_bool_accepts_common_enabled_values() {
        for value in ["true", "TRUE", "1", "yes", "on"] {
            assert!(parse_bool(value));
        }
    }

    #[test]
    fn parse_bool_treats_unknown_values_as_disabled() {
        for value in ["false", "0", "no", "off", "", "definitely"] {
            assert!(!parse_bool(value));
        }
    }

    #[test]
    fn auto_accept_delay_is_clamped() {
        assert_eq!(clamp_auto_accept_delay(500), 500);
        assert_eq!(clamp_auto_accept_delay(99_999), 10_000);
    }

    fn assert_no_temp_files_for(path: &Path) {
        let parent = path.parent().expect("temp path should have parent");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("temp path should have file name");
        let leftovers = fs::read_dir(parent)
            .expect("temp dir should read")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(file_name) && name.ends_with(".tmp"))
            })
            .count();
        assert_eq!(leftovers, 0);
    }
}
