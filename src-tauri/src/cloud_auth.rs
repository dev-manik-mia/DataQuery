//! Short-lived database tokens from the cloud providers' own CLIs. The CLIs
//! handle credential chains, SSO and MFA; DataQuery only runs them and uses the
//! token as the password for this connection attempt. Tokens are never stored.

use std::path::PathBuf;
use std::time::Duration;

use dq_core::config::{AuthMethod, ConnConfig};
use dq_core::{DbError, DbResult, ErrorKind};
use tokio::process::Command;

const CLI_TIMEOUT: Duration = Duration::from_secs(60);

/// Directories searched for the CLIs in addition to `PATH`: apps started from
/// the Finder or a desktop launcher often get a minimal `PATH`.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join("google-cloud-sdk/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

fn find_cli(name: &str) -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(windows) { &[".cmd", ".exe", ""] } else { &[""] };
    search_dirs().into_iter().find_map(|dir| {
        candidates.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|p| p.is_file())
    })
}

async fn run(cli: &str, args: &[&str], install_hint: &str) -> DbResult<String> {
    let path = find_cli(cli).ok_or_else(|| {
        DbError::new(ErrorKind::Auth, format!("The `{cli}` command was not found.")).with_action(Some(install_hint))
    })?;
    let output = tokio::time::timeout(CLI_TIMEOUT, Command::new(&path).args(args).kill_on_drop(true).output())
        .await
        .map_err(|_| DbError::new(ErrorKind::Timeout, format!("`{cli}` did not answer within 60 seconds.")))?
        .map_err(|e| DbError::new(ErrorKind::Auth, format!("Cannot run `{cli}`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("no error output");
        return Err(DbError::new(ErrorKind::Auth, format!("`{cli}` failed: {first}"))
            .with_action(Some(format!("Sign in with the {cli} CLI, then connect again."))));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() {
        return Err(DbError::new(ErrorKind::Auth, format!("`{cli}` returned no token.")));
    }
    Ok(token)
}

/// The password to use for `config`: a fresh token for cloud IAM, else `saved`.
pub async fn password(config: &ConnConfig, saved: Option<String>) -> DbResult<Option<String>> {
    let port = config.port.to_string();
    Ok(Some(match &config.auth {
        AuthMethod::Password => return Ok(saved),
        AuthMethod::AwsIam { region, profile } => {
            let mut args = vec![
                "rds",
                "generate-db-auth-token",
                "--hostname",
                &config.host,
                "--port",
                &port,
                "--region",
                region,
                "--username",
                &config.username,
            ];
            if let Some(profile) = profile {
                args.extend(["--profile", profile]);
            }
            run("aws", &args, "Install the AWS CLI and run `aws configure` or `aws sso login`.").await?
        }
        AuthMethod::AzureAd => {
            run(
                "az",
                &["account", "get-access-token", "--resource-type", "oss-rdbms", "--query", "accessToken", "-o", "tsv"],
                "Install the Azure CLI and run `az login`.",
            )
            .await?
        }
        AuthMethod::GcpIam => {
            run("gcloud", &["sql", "generate-login-token"], "Install the Google Cloud CLI and run `gcloud auth login`.")
                .await?
        }
    }))
}
