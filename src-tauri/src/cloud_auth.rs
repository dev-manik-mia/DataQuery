//! Short-lived database tokens from the cloud providers' own CLIs. The CLIs
//! handle credential chains, SSO and MFA; DataQuery only runs them and uses the
//! token as the password for this connection attempt. Tokens are never stored.

use dq_core::DbResult;
use dq_core::config::{AuthMethod, ConnConfig};
use dq_core::engine::Engine;
use dq_core::programs::cloud_cli_token as run;

/// The password to use for `config`: a fresh token for cloud IAM, else `saved`.
pub async fn password(config: &ConnConfig, saved: Option<String>) -> DbResult<Option<String>> {
    let port = config.port.to_string();
    Ok(Some(match &config.auth {
        AuthMethod::Password => return Ok(saved),
        // BigQuery sessions outlive a token: the adapter runs `gcloud` itself and refreshes.
        AuthMethod::GcpIam if config.engine == Engine::Bigquery => return Ok(None),
        // DynamoDB signs every request itself: it needs the profile's credentials,
        // passed to the adapter as the CLI's credential-process JSON.
        AuthMethod::AwsIam { profile, .. } if config.engine == Engine::Dynamodb => {
            let mut args = vec!["configure", "export-credentials", "--format", "process"];
            if let Some(profile) = profile {
                args.extend(["--profile", profile]);
            }
            run("aws", &args, "Install the AWS CLI and run `aws configure` or `aws sso login`.").await?
        }
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
