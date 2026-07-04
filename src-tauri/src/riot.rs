use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

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
    for name in RIOT_PROCESS_NAMES {
        let image_name = format!("{name}.exe");
        let output = Command::new("taskkill")
            .args(["/FI", &format!("IMAGENAME eq {image_name}"), "/T", "/F"])
            .output()
            .with_context(|| format!("Unable to run taskkill for {name}"))?;
        ensure_taskkill_result(name, &output)?;
    }

    Ok(())
}

fn ensure_taskkill_result(name: &str, output: &Output) -> Result<()> {
    if output.status.success() || taskkill_output_means_no_process(output) {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    Err(anyhow!(
        "Unable to stop {name}: taskkill exited with {}{}",
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(" ({detail})")
        }
    ))
}

fn taskkill_output_means_no_process(output: &Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stdout.contains("no tasks are running")
        || stderr.contains("not found")
        || stderr.contains("no running instance")
}

pub fn running_riot_processes() -> Result<Vec<String>> {
    let mut running = Vec::new();
    for name in RIOT_PROCESS_NAMES {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {name}.exe")])
            .output()
            .with_context(|| format!("Unable to run tasklist for {name}"))?;
        if !output.status.success() {
            return Err(anyhow!(
                "Unable to query {name}: tasklist exited with {}",
                output.status
            ));
        }
        if tasklist_output_has_process(name, &output) {
            running.push(name.to_string());
        }
    }
    Ok(running)
}

fn tasklist_output_has_process(name: &str, output: &Output) -> bool {
    String::from_utf8_lossy(&output.stdout)
        .to_ascii_lowercase()
        .contains(&format!("{}.exe", name.to_ascii_lowercase()))
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
    use std::os::windows::process::ExitStatusExt;

    fn command_output(exit_code: u32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(exit_code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

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
    fn taskkill_output_treats_no_matching_tasks_as_ok() {
        let output = command_output(
            128,
            "INFO: No tasks are running which match the specified criteria.\r\n",
            "",
        );

        assert!(taskkill_output_means_no_process(&output));
        assert!(ensure_taskkill_result("LeagueClient", &output).is_ok());
    }

    #[test]
    fn taskkill_output_reports_real_failure() {
        let output = command_output(5, "", "ERROR: Access is denied.\r\n");

        assert!(!taskkill_output_means_no_process(&output));
        let error = ensure_taskkill_result("LeagueClient", &output)
            .expect_err("access denied should fail")
            .to_string();
        assert!(error.contains("LeagueClient"));
        assert!(error.contains("Access is denied"));
    }

    #[test]
    fn tasklist_output_detects_matching_process() {
        let output = command_output(
            0,
            "Image Name                     PID Session Name        Session#    Mem Usage\r\nRiotClientServices.exe       123 Console                    1     42,000 K\r\n",
            "",
        );

        assert!(tasklist_output_has_process("RiotClientServices", &output));
        assert!(!tasklist_output_has_process("LeagueClient", &output));
    }

    #[test]
    fn riot_process_list_includes_client_ui_processes() {
        assert!(RIOT_PROCESS_NAMES.contains(&"RiotClientUx"));
        assert!(RIOT_PROCESS_NAMES.contains(&"LeagueClientUx"));
        assert!(RIOT_PROCESS_NAMES.contains(&"LeagueClientUxRender"));
    }

    #[test]
    fn tasklist_output_detects_riot_client_ui_process() {
        let output = command_output(
            0,
            "Image Name                     PID Session Name        Session#    Mem Usage\r\nRiotClientUx.exe             456 Console                    1     80,000 K\r\n",
            "",
        );

        assert!(tasklist_output_has_process("RiotClientUx", &output));
        assert!(!tasklist_output_has_process("RiotClientServices", &output));
    }

    #[test]
    fn tasklist_output_ignores_no_matching_tasks() {
        let output = command_output(
            0,
            "INFO: No tasks are running which match the specified criteria.\r\n",
            "",
        );

        assert!(!tasklist_output_has_process("RiotClientServices", &output));
    }
}
