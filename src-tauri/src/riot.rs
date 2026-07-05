use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use sysinfo::{Signal, System};

use crate::models::LaunchGame;

const RIOT_PROCESS_NAMES: [&str; 8] = [
    "LeagueClient",
    "LeagueClientUx",
    "LeagueClientUxRender",
    "LoR",
    "RiotClientCrashHandler",
    "VALORANT-Win64-Shipping",
    "RiotClientServices",
    "RiotClientUx",
];

pub fn riot_client_path() -> Option<PathBuf> {
    install_config_paths()
        .into_iter()
        .find_map(|path| riot_client_path_from_config(&path))
        .or_else(riot_client_path_from_common_installs)
}

pub fn launch_riot_client(
    riot_client: &Path,
    config_port: u16,
    game: LaunchGame,
    game_patchline: &str,
    riot_client_params: Option<&str>,
    game_params: Option<&str>,
) -> Result<()> {
    Command::new(riot_client)
        .args(build_launch_args(
            config_port,
            game,
            game_patchline,
            riot_client_params,
            game_params,
        )?)
        .spawn()
        .with_context(|| format!("Unable to launch {}", riot_client.display()))?;

    Ok(())
}

pub fn validate_launch_params(
    game: LaunchGame,
    game_patchline: &str,
    riot_client_params: Option<&str>,
    game_params: Option<&str>,
) -> Result<()> {
    if game.launch_product().is_some() {
        validate_game_patchline(game_patchline)?;
    }
    if let Some(params) = riot_client_params.filter(|p| !p.trim().is_empty()) {
        parse_launch_args(params)?;
    }
    if let Some(params) = game_params.filter(|p| !p.trim().is_empty()) {
        parse_launch_args(params)?;
    }
    Ok(())
}

fn build_launch_args(
    config_port: u16,
    game: LaunchGame,
    game_patchline: &str,
    riot_client_params: Option<&str>,
    game_params: Option<&str>,
) -> Result<Vec<String>> {
    let game_patchline = validate_game_patchline(game_patchline)?;
    let mut args = vec![format!(
        "--client-config-url=http://127.0.0.1:{config_port}"
    )];

    if let Some(product) = game.launch_product() {
        args.push(format!("--launch-product={product}"));
        args.push(format!("--launch-patchline={game_patchline}"));
    }

    if let Some(params) = riot_client_params.filter(|p| !p.trim().is_empty()) {
        args.extend(parse_launch_args(params)?);
    }

    if let Some(params) = game_params.filter(|p| !p.trim().is_empty()) {
        args.push("--".to_string());
        args.extend(parse_launch_args(params)?);
    }

    Ok(args)
}

fn validate_game_patchline(game_patchline: &str) -> Result<&str> {
    let game_patchline = game_patchline.trim();
    if game_patchline.is_empty() {
        return Err(anyhow!("Game patchline cannot be empty"));
    }
    if game_patchline.chars().any(char::is_whitespace) {
        return Err(anyhow!("Game patchline cannot contain whitespace"));
    }
    Ok(game_patchline)
}

pub fn kill_riot_processes() -> Result<()> {
    let system = System::new_all();
    let mut failures = Vec::new();

    for process in system.processes().values() {
        if !process_name_matches_riot_target(&process.name().to_string_lossy()) {
            continue;
        }
        let killed = process
            .kill_with(Signal::Kill)
            .unwrap_or_else(|| process.kill());
        if !killed {
            failures.push(process.name().to_string_lossy().to_string());
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        failures.sort();
        failures.dedup();
        Err(anyhow!(
            "Unable to stop Riot processes: {}",
            failures.join(", ")
        ))
    }
}

pub fn running_riot_processes() -> Result<Vec<String>> {
    let system = System::new_all();
    let mut running = system
        .processes()
        .values()
        .filter_map(|process| {
            let process_name = process.name().to_string_lossy();
            let normalized = process_name.to_ascii_lowercase();
            riot_process_targets()
                .iter()
                .position(|target| target == &normalized)
                .map(|index| RIOT_PROCESS_NAMES[index].to_string())
        })
        .collect::<Vec<_>>();
    running.sort();
    running.dedup();
    Ok(running)
}

fn riot_process_targets() -> Vec<String> {
    RIOT_PROCESS_NAMES
        .into_iter()
        .map(|name| format!("{}.exe", name.to_ascii_lowercase()))
        .collect()
}

fn process_name_matches_riot_target(name: &str) -> bool {
    let targets = riot_process_targets();
    let normalized = name.to_ascii_lowercase();
    targets.iter().any(|target| target == &normalized)
}

pub fn ensure_riot_client() -> Result<PathBuf> {
    riot_client_path().ok_or_else(|| {
        anyhow!(
            "Unable to find RiotClientServices.exe. Expected RiotClientInstalls.json under ProgramData or a Riot Client install under C:\\Riot Games."
        )
    })
}

fn install_config_paths() -> Vec<PathBuf> {
    ["PROGRAMDATA", "LOCALAPPDATA", "APPDATA"]
        .into_iter()
        .filter_map(|var| env::var_os(var).map(PathBuf::from))
        .map(|dir| dir.join("Riot Games").join("RiotClientInstalls.json"))
        .collect()
}

fn riot_client_path_from_config(path: &Path) -> Option<PathBuf> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;

    ["rc_default", "rc_live", "rc_beta"]
        .iter()
        .filter_map(|key| value.get(key)?.as_str())
        .map(riot_client_executable_candidate)
        .find(|path| path.exists())
        .or_else(|| {
            value
                .get("patchlines")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|items| items.values())
                .filter_map(Value::as_str)
                .map(riot_client_executable_candidate)
                .find(|path| path.exists())
        })
        .or_else(|| {
            value
                .get("associated_client")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|items| items.values())
                .filter_map(Value::as_str)
                .map(riot_client_executable_candidate)
                .find(|path| path.exists())
        })
}

fn riot_client_path_from_common_installs() -> Option<PathBuf> {
    ["C:", "D:", "B:", "E:", "F:"]
        .into_iter()
        .map(|drive| {
            PathBuf::from(format!(
                "{drive}\\Riot Games\\Riot Client\\RiotClientServices.exe"
            ))
        })
        .find(|path| path.exists())
}

fn normalize_riot_path(path: &str) -> PathBuf {
    PathBuf::from(path.replace('/', "\\"))
}

fn riot_client_executable_candidate(path: &str) -> PathBuf {
    let path = normalize_riot_path(path);
    if path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("RiotClientServices.exe"))
    {
        path
    } else {
        path.join("RiotClientServices.exe")
    }
}

fn parse_launch_args(value: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars().peekable();
    let mut quote = None;
    let mut has_current = false;

    while let Some(ch) = chars.next() {
        match (ch, quote) {
            ('\\', Some(active)) => {
                if matches!(chars.peek(), Some(next) if *next == active || *next == '\\') {
                    current.push(chars.next().expect("peeked next char exists"));
                } else {
                    current.push(ch);
                }
                has_current = true;
            }
            ('"' | '\'', None) => {
                quote = Some(ch);
                has_current = true;
            }
            (ch, Some(active)) if ch == active => {
                quote = None;
                has_current = true;
            }
            (ch, None) if ch.is_whitespace() => {
                if has_current {
                    args.push(std::mem::take(&mut current));
                    has_current = false;
                }
            }
            _ => {
                current.push(ch);
                has_current = true;
            }
        }
    }

    if let Some(quote) = quote {
        return Err(anyhow!("Unclosed quote in launch parameters: {quote}"));
    }

    if has_current {
        args.push(current);
    }

    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn riot_client_executable_candidate_keeps_direct_exe_path() {
        assert_eq!(
            riot_client_executable_candidate("C:\\Riot Games\\Riot Client\\RiotClientServices.exe"),
            PathBuf::from("C:\\Riot Games\\Riot Client\\RiotClientServices.exe")
        );
    }

    #[test]
    fn riot_client_executable_candidate_accepts_install_directory() {
        assert_eq!(
            riot_client_executable_candidate("C:\\Riot Games\\Riot Client"),
            PathBuf::from("C:\\Riot Games\\Riot Client\\RiotClientServices.exe")
        );
    }

    #[test]
    fn riot_client_executable_candidate_normalizes_forward_slashes() {
        assert_eq!(
            riot_client_executable_candidate("C:/Riot Games/Riot Client"),
            PathBuf::from("C:\\Riot Games\\Riot Client\\RiotClientServices.exe")
        );
    }

    #[test]
    fn parse_launch_args_splits_plain_args() {
        assert_eq!(
            parse_launch_args("--one --two=value").expect("args should parse"),
            vec!["--one", "--two=value"]
        );
    }

    #[test]
    fn parse_launch_args_preserves_quoted_values() {
        assert_eq!(
            parse_launch_args("--install-directory \"C:\\Riot Games\\League of Legends\"")
                .expect("args should parse"),
            vec!["--install-directory", "C:\\Riot Games\\League of Legends"]
        );
    }

    #[test]
    fn parse_launch_args_handles_single_quotes() {
        assert_eq!(
            parse_launch_args("--flag 'two words'").expect("args should parse"),
            vec!["--flag", "two words"]
        );
    }

    #[test]
    fn parse_launch_args_handles_escaped_quote_inside_quotes() {
        assert_eq!(
            parse_launch_args("--name \"Ghosty \\\"Test\\\"\"").expect("args should parse"),
            vec!["--name", "Ghosty \"Test\""]
        );
    }

    #[test]
    fn parse_launch_args_preserves_empty_quoted_value() {
        assert_eq!(
            parse_launch_args("--flag \"\" --next").expect("args should parse"),
            vec!["--flag", "", "--next"]
        );
    }

    #[test]
    fn parse_launch_args_ignores_plain_whitespace() {
        assert_eq!(
            parse_launch_args("  \t  ").expect("args should parse"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn parse_launch_args_rejects_unclosed_quotes() {
        assert!(parse_launch_args("--flag \"unfinished").is_err());
    }

    #[test]
    fn validate_launch_params_rejects_bad_riot_client_params() {
        assert!(
            validate_launch_params(LaunchGame::Lol, "live", Some("--flag \"unfinished"), None)
                .is_err()
        );
    }

    #[test]
    fn validate_launch_params_rejects_bad_game_params() {
        assert!(
            validate_launch_params(LaunchGame::Lol, "live", None, Some("--flag \"unfinished"))
                .is_err()
        );
    }

    #[test]
    fn validate_launch_params_rejects_empty_or_spaced_patchline() {
        for patchline in ["", "   ", "live beta"] {
            assert!(validate_launch_params(LaunchGame::Lol, patchline, None, None).is_err());
        }
    }

    #[test]
    fn validate_launch_params_allows_blank_patchline_for_riot_client_only() {
        assert!(validate_launch_params(LaunchGame::RiotClient, "", None, None).is_ok());
    }

    #[test]
    fn build_launch_args_trims_patchline() {
        let args = build_launch_args(49232, LaunchGame::Lol, " live ", None, None)
            .expect("launch args should build");

        assert!(args.contains(&"--launch-patchline=live".to_string()));
        assert!(!args.contains(&"--launch-patchline= live ".to_string()));
    }

    #[test]
    fn build_launch_args_matches_expected_riot_and_game_sections() {
        assert_eq!(
            build_launch_args(
                49232,
                LaunchGame::Lol,
                "live",
                Some("--client-flag value"),
                Some("--game-flag \"two words\""),
            )
            .expect("launch args should build"),
            vec![
                "--client-config-url=http://127.0.0.1:49232",
                "--launch-product=league_of_legends",
                "--launch-patchline=live",
                "--client-flag",
                "value",
                "--",
                "--game-flag",
                "two words",
            ]
        );
    }

    #[test]
    fn riot_process_list_includes_client_ui_processes() {
        assert!(RIOT_PROCESS_NAMES.contains(&"RiotClientUx"));
        assert!(RIOT_PROCESS_NAMES.contains(&"LeagueClientUx"));
        assert!(RIOT_PROCESS_NAMES.contains(&"LeagueClientUxRender"));
    }

    #[test]
    fn riot_process_matcher_detects_client_ui_processes() {
        assert!(process_name_matches_riot_target("RiotClientUx.exe"));
        assert!(process_name_matches_riot_target("LeagueClientUx.exe"));
        assert!(process_name_matches_riot_target("LeagueClientUxRender.exe"));
    }

    #[test]
    fn riot_process_matcher_is_case_insensitive_and_exact() {
        assert!(process_name_matches_riot_target("riotclientservices.exe"));
        assert!(process_name_matches_riot_target(
            "VALORANT-Win64-Shipping.exe"
        ));
        assert!(!process_name_matches_riot_target(
            "RiotClientServicesHelper.exe"
        ));
        assert!(!process_name_matches_riot_target("LeagueClient"));
    }
}
