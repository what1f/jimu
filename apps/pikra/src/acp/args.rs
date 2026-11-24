use crate::acp::{AcpConfig, auth};
use pikra_kit::WalkBoundary;
use std::collections::HashMap;

pub fn parse_walk_boundary(value: &str) -> Result<WalkBoundary, String> {
    match value {
        "project-root" => Ok(WalkBoundary::ProjectRoot),
        "filesystem-root" => Ok(WalkBoundary::FilesystemRoot),
        "cwd-only" => Ok(WalkBoundary::CurrentDirOnly),
        _ => Err("Use project-root, filesystem-root, or cwd-only for --walk-boundary.".into()),
    }
}

pub fn parse_instruction_max_bytes(value: &str) -> Result<Option<usize>, String> {
    match value {
        "none" | "unlimited" => Ok(None),
        _ => value.parse().map(Some).map_err(|_| {
            "Use a nonnegative byte count or none for --instruction-max-bytes.".into()
        }),
    }
}

pub async fn login(config: &AcpConfig) -> Result<(), String> {
    auth::login(&config.pikra_home).await
}

pub async fn logout(config: &AcpConfig) -> Result<(), String> {
    auth::Auth::new(config.clone())
        .logout()
        .await
        .map_err(|error| error.message)?;
    eprintln!("Signed out of ChatGPT.");
    Ok(())
}

pub(crate) fn auth_env(config: &AcpConfig) -> HashMap<String, String> {
    HashMap::from([(
        "PIKRA_HOME".into(),
        config.pikra_home.to_string_lossy().into_owned(),
    )])
}
