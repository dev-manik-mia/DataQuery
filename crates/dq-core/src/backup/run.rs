//! Runs a dump or restore tool: streams its messages into progress, feeds
//! restore files to its input, kills its process group on cancel and turns a
//! failure into a readable error with the tool's last lines.

use std::collections::VecDeque;
use std::io::Write;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::BackupFamily;
use super::args::{Invocation, uri_encode};
use super::tools::Found;
use crate::error::{DbError, DbResult, ErrorKind};
use crate::programs;
use crate::redact::MASK;
use crate::transfer::{Progress, TransferControl};

const TICK: Duration = Duration::from_millis(250);
/// Tool lines kept for the error message.
const TAIL_LINES: usize = 12;
const MAX_WARNINGS: usize = 20;
/// Environment that could send a tool elsewhere or reuse another password.
const CLEARED_ENV: &[&str] = &["PGPASSWORD", "PGSERVICE", "PGSERVICEFILE", "PGHOSTADDR", "MYSQL_PWD", "MYSQL_HOST"];

#[derive(Debug)]
pub(crate) struct Outcome {
    pub objects: u64,
    pub bytes: u64,
    pub warnings: Vec<String>,
    /// E.g. `pg_dump 18.4`.
    pub tool: Option<String>,
}

/// What a line of tool output means.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Line<'a> {
    /// A table, collection or other object was dumped or restored.
    Object(&'a str),
    Warning(&'a str),
    Error(&'a str),
    Info(&'a str),
}

/// Classifies one line of a tool's standard error.
pub(crate) fn classify(family: BackupFamily, line: &str) -> Line<'_> {
    let line = line.trim();
    let lower = line.to_ascii_lowercase();
    match family {
        BackupFamily::Postgres => {
            // `pg_dump: dumping contents of table "public.a"`, `psql:<stdin>:12: ERROR: …`.
            let text =
                ["pg_dump: ", "pg_restore: ", "psql: "].iter().find_map(|p| line.strip_prefix(p)).unwrap_or(line);
            if lower.contains("error:") || lower.contains("fatal:") {
                Line::Error(text)
            } else if lower.contains("warning:") {
                Line::Warning(text)
            } else if text.starts_with("dumping contents of table")
                // pg_dump also says "creating" for plain output; only pg_restore creates objects.
                || (line.starts_with("pg_restore: ")
                    && ["processing data for table", "creating "].iter().any(|m| text.starts_with(m)))
            {
                Line::Object(text)
            } else {
                Line::Info(text)
            }
        }
        BackupFamily::Mysql => {
            // `-- Retrieving table structure for table bk_a...`, `ERROR 1050 (42S01) at line 25: …`.
            let text = line.trim_start_matches("-- ");
            if lower.starts_with("error") || lower.contains(": got error") || lower.contains("[error]") {
                Line::Error(text)
            } else if lower.contains("[warning]") {
                Line::Warning(text)
            } else if text.starts_with("Retrieving table structure for table") {
                Line::Object(text)
            } else {
                Line::Info(text)
            }
        }
        BackupFamily::Mongodb => {
            // `2025-10-20T10:00:00.000+0200\tdone dumping app.users (3 documents)`.
            let text = line.rsplit('\t').next().unwrap_or(line).trim();
            if lower.contains("failed:") || lower.contains("error") {
                Line::Error(text)
            } else if text.starts_with("done dumping") || text.starts_with("finished restoring") {
                Line::Object(text)
            } else if text.contains("document(s) failed to restore") && !text.starts_with("0 document") {
                Line::Warning(text)
            } else {
                Line::Info(text)
            }
        }
        BackupFamily::Sqlite => Line::Info(line),
    }
}

/// What the tool said so far.
#[derive(Default)]
struct Messages {
    objects: u64,
    current: String,
    tail: VecDeque<String>,
    warnings: Vec<String>,
    last_error: Option<String>,
}

impl Messages {
    fn add(&mut self, family: BackupFamily, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        match classify(family, line) {
            Line::Object(text) => {
                self.objects += 1;
                text.clone_into(&mut self.current);
            }
            Line::Warning(text) if self.warnings.len() < MAX_WARNINGS => self.warnings.push(text.to_owned()),
            Line::Error(text) => self.last_error = Some(text.to_owned()),
            Line::Info(text) | Line::Warning(text) => text.clone_into(&mut self.current),
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line.trim().to_owned());
    }
}

/// Removes the password (and its URI-encoded form) from tool output.
fn mask(line: &str, secrets: &[String]) -> String {
    secrets.iter().fold(line.to_owned(), |text, secret| text.replace(secret.as_str(), MASK))
}

/// Error category from the tool's last error line.
fn error_kind(text: &str) -> (ErrorKind, Option<&'static str>) {
    let t = text.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    if has(&["server version mismatch"]) {
        let action = "Use a pg_dump at least as new as the server: install it, or choose it in Settings → Engines.";
        (ErrorKind::Unsupported, Some(action))
    } else if has(&["password authentication failed", "access denied", "authentication failed", "auth error"]) {
        (ErrorKind::Auth, None)
    } else if has(&["ssl", "tls", "certificate", "x509"]) {
        (ErrorKind::Tls, None)
    } else if has(&[
        "could not connect",
        "connection refused",
        "can't connect",
        "could not translate host",
        "unknown mysql server host",
        "server selection",
        "no reachable servers",
        "timed out",
    ]) {
        (ErrorKind::Network, None)
    } else if has(&["permission denied", "must be owner", "insufficient privilege", "not authorized", "command denied"])
    {
        (ErrorKind::Permission, None)
    } else if has(&["no space left", "disk full"]) {
        (ErrorKind::Storage, None)
    } else {
        (ErrorKind::Server, None)
    }
}

fn failure(found: &Found, status: ExitStatus, messages: &Messages) -> DbError {
    let how = status.code().map_or_else(|| "it was stopped".to_owned(), |c| format!("exit code {c}"));
    let line = messages.last_error.as_deref().or_else(|| messages.tail.back().map(String::as_str));
    let (kind, action) = error_kind(line.unwrap_or_default());
    let message = match line {
        Some(line) => format!("{} failed ({how}): {line}", found.label()),
        None => format!("{} failed ({how}) without a message.", found.label()),
    };
    let tail = messages.tail.iter().cloned().collect::<Vec<_>>().join("\n");
    DbError::new(kind, message).with_detail((!tail.is_empty()).then_some(tail)).with_action(action)
}

/// A private file (mode 0600 on Unix) with `contents`, deleted when dropped.
fn private_file(contents: &str) -> DbResult<tempfile::NamedTempFile> {
    let failed = |e: std::io::Error| DbError::new(ErrorKind::Storage, format!("Cannot write a temporary file: {e}"));
    let mut file = tempfile::Builder::new().prefix("dataquery-").tempfile().map_err(failed)?;
    file.write_all(contents.as_bytes()).and_then(|()| file.flush()).map_err(failed)?;
    Ok(file)
}

/// Ends the tool and anything it started.
async fn kill(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // The tool leads its own process group (see `run`), so this reaches pg_dump's workers too.
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
    }
    let _ = child.kill().await;
}

/// Runs the tool to completion, reporting progress and honouring cancel.
pub(crate) async fn run(
    found: &Found,
    invocation: Invocation,
    password: Option<&str>,
    control: &TransferControl,
) -> DbResult<Outcome> {
    control.check()?;
    let family = invocation.family;
    let option_file =
        invocation.option_file.as_ref().map(|(flag, contents)| Ok((*flag, private_file(contents)?))).transpose()?;
    let mut command = programs::command(&found.path);
    if let Some((flag, file)) = &option_file {
        let mut arg = std::ffi::OsString::from(flag);
        arg.push(file.path());
        command.arg(arg);
    }
    command.args(&invocation.args);
    for name in CLEARED_ENV {
        command.env_remove(name);
    }
    command.envs(invocation.env.iter().map(|(k, v)| (k, v)));
    command.stdin(if invocation.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| DbError::new(ErrorKind::Storage, format!("Cannot start {}: {e}", found.path.display())))?;

    let secrets: Vec<String> =
        password.filter(|p| p.len() >= 3).map(|p| vec![p.to_owned(), uri_encode(p)]).unwrap_or_default();
    let messages = Arc::new(Mutex::new(Messages::default()));
    let stderr = child.stderr.take().expect("stderr is piped");
    let reader = {
        let messages = messages.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr);
            let mut buf = Vec::new();
            while lines.read_until(b'\n', &mut buf).await.is_ok_and(|n| n > 0) {
                let line = mask(&String::from_utf8_lossy(&buf), &secrets);
                messages.lock().unwrap_or_else(PoisonError::into_inner).add(family, &line);
                buf.clear();
            }
        })
    };

    // Restores read their file through standard input, which counts the bytes.
    let fed = Arc::new(AtomicU64::new(0));
    let mut total = None;
    let feeder = match (&invocation.stdin, child.stdin.take()) {
        (Some(path), Some(mut stdin)) => {
            let unreadable =
                |e: std::io::Error| DbError::new(ErrorKind::Storage, format!("Cannot read {}: {e}", path.display()));
            let mut file = match tokio::fs::File::open(path).await {
                Ok(file) => file,
                Err(e) => {
                    kill(&mut child).await;
                    return Err(unreadable(e));
                }
            };
            total = file.metadata().await.ok().map(|m| m.len());
            let fed = fed.clone();
            Some(tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                // A write fails when the tool exits early; its exit status tells why.
                while let Ok(n @ 1..) = file.read(&mut buf).await {
                    if stdin.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                    fed.fetch_add(n as u64, Ordering::Relaxed);
                }
                let _ = stdin.shutdown().await;
            }))
        }
        _ => None,
    };

    let bytes = |fed: &AtomicU64| match &invocation.output {
        Some(out) => super::disk_size(out),
        None => fed.load(Ordering::Relaxed),
    };
    let mut tick = tokio::time::interval(TICK);
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            _ = tick.tick() => {
                if control.is_cancelled() {
                    kill(&mut child).await;
                    if let Some(feeder) = feeder {
                        feeder.abort();
                    }
                    reader.abort();
                    return Err(DbError::new(ErrorKind::Cancelled, "Cancelled."));
                }
                let m = messages.lock().unwrap_or_else(PoisonError::into_inner);
                control.report(&Progress {
                    rows: m.objects,
                    bytes: bytes(&fed),
                    total_bytes: total,
                    detail: Some(m.current.as_str()).filter(|d| !d.is_empty()),
                });
            }
        }
    }
    .map_err(|e| DbError::internal(format!("Lost track of {}: {e}", found.label())))?;
    if let Some(feeder) = feeder {
        feeder.abort();
    }
    // A child the tool left behind may hold standard error open; don't wait for it forever.
    let _ = tokio::time::timeout(Duration::from_secs(5), reader).await;
    drop(option_file);
    let messages = std::mem::take(&mut *messages.lock().unwrap_or_else(PoisonError::into_inner));
    if !status.success() {
        return Err(failure(found, status, &messages));
    }
    let mut warnings = invocation.warnings;
    warnings.extend(messages.warnings);
    Ok(Outcome { objects: messages.objects, bytes: bytes(&fed), warnings, tool: Some(found.label()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_lines_become_progress() {
        use BackupFamily::{Mongodb, Mysql, Postgres};
        assert_eq!(
            classify(Postgres, "pg_dump: dumping contents of table \"public.bk_a\"\n"),
            Line::Object("dumping contents of table \"public.bk_a\"")
        );
        assert_eq!(
            classify(Postgres, "pg_restore: creating TABLE \"public.bk_a\""),
            Line::Object("creating TABLE \"public.bk_a\"")
        );
        assert_eq!(
            classify(Postgres, "pg_restore: processing data for table \"public.bk_a\""),
            Line::Object("processing data for table \"public.bk_a\"")
        );
        assert_eq!(classify(Postgres, "pg_dump: reading extensions"), Line::Info("reading extensions"));
        assert_eq!(
            classify(Postgres, "pg_dump: creating TABLE \"public.a\""),
            Line::Info("creating TABLE \"public.a\"")
        );
        assert!(matches!(
            classify(Postgres, "pg_dump: error: aborting because of server version mismatch"),
            Line::Error(_)
        ));
        assert!(matches!(classify(Postgres, "psql:<stdin>:12: ERROR:  relation \"x\" already exists"), Line::Error(_)));
        assert!(matches!(
            classify(Postgres, "pg_dump: warning: there are circular foreign-key constraints"),
            Line::Warning(_)
        ));

        assert_eq!(
            classify(Mysql, "-- Retrieving table structure for table bk_a..."),
            Line::Object("Retrieving table structure for table bk_a...")
        );
        assert_eq!(classify(Mysql, "-- Sending SELECT query..."), Line::Info("Sending SELECT query..."));
        assert!(matches!(
            classify(Mysql, "ERROR 1050 (42S01) at line 25: Table 'bk_a' already exists"),
            Line::Error(_)
        ));
        assert!(matches!(classify(Mysql, "mysqldump: Got error: 1045: Access denied for user"), Line::Error(_)));
        assert!(matches!(classify(Mysql, "mysqldump: [Warning] something"), Line::Warning(_)));

        assert_eq!(
            classify(Mongodb, "2025-10-20T10:00:00.000+0200\tdone dumping app.users (3 documents)"),
            Line::Object("done dumping app.users (3 documents)")
        );
        assert_eq!(
            classify(Mongodb, "2025-10-20T10:00:00.000+0200\tfinished restoring app.users (3 documents, 0 failures)"),
            Line::Object("finished restoring app.users (3 documents, 0 failures)")
        );
        assert!(matches!(
            classify(
                Mongodb,
                "2025-10-20T10:00:00.000+0200\t0 document(s) restored successfully. 0 document(s) failed to restore."
            ),
            Line::Info(_)
        ));
        assert!(matches!(
            classify(Mongodb, "x\t3 document(s) restored successfully. 2 document(s) failed to restore."),
            Line::Warning(_)
        ));
        assert!(matches!(classify(Mongodb, "x\tFailed: can't create session: connection refused"), Line::Error(_)));
    }

    #[test]
    fn messages_keep_a_tail_and_count_objects() {
        let mut m = Messages::default();
        for i in 0..20 {
            m.add(BackupFamily::Postgres, &format!("pg_dump: dumping contents of table \"t{i}\""));
        }
        m.add(BackupFamily::Postgres, "  ");
        m.add(BackupFamily::Postgres, "pg_dump: error: query failed");
        assert_eq!(m.objects, 20);
        assert_eq!(m.tail.len(), TAIL_LINES);
        assert_eq!(m.tail.back().map(String::as_str), Some("pg_dump: error: query failed"));
        assert_eq!(m.last_error.as_deref(), Some("error: query failed"));
    }

    #[test]
    fn passwords_are_masked_and_errors_categorised() {
        let secrets = vec!["p@ss".to_owned(), uri_encode("p@ss")];
        assert_eq!(mask("uri mongodb://u:p%40ss@h and p@ss", &secrets), "uri mongodb://u:***@h and ***");
        assert_eq!(error_kind("FATAL:  password authentication failed for user \"x\"").0, ErrorKind::Auth);
        assert_eq!(error_kind("Got error: 2003: Can't connect to MySQL server").0, ErrorKind::Network);
        let (kind, action) = error_kind("error: aborting because of server version mismatch");
        assert_eq!(kind, ErrorKind::Unsupported);
        assert!(action.unwrap().contains("Settings"));
        assert_eq!(error_kind("permission denied for table x").0, ErrorKind::Permission);
    }

    #[cfg(unix)]
    fn script(dir: &std::path::Path, body: &str) -> Found {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("tool");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        Found {
            tool: super::super::Tool::PgDump,
            path,
            version: super::super::ToolVersion::parse("pg_dump 18.1").unwrap(),
        }
    }

    #[cfg(unix)]
    fn invocation(stdin: Option<std::path::PathBuf>, option_file: Option<&str>) -> Invocation {
        Invocation {
            family: BackupFamily::Postgres,
            args: vec!["arg one".into()],
            env: vec![("PGPASSWORD", "hunter22".into())],
            option_file: option_file.map(|c| ("--defaults-extra-file=", c.to_owned())),
            stdin,
            output: None,
            warnings: vec!["from the plan".into()],
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_tools_feeds_input_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        // Echo what the tool receives: its option file (still present while it runs), args, env and input.
        let out = dir.path().join("seen");
        let found = script(
            dir.path(),
            &format!(
                "f=${{1#--defaults-extra-file=}}; ls -l \"$f\" | cut -c1-10 > {o}; cat \"$f\" >> {o}; echo \"$2\" >> {o}; echo \"$PGPASSWORD\" >> {o}; cat >> {o}\necho 'pg_dump: dumping contents of table \"t\"' >&2\necho 'pg_dump: warning: careful' >&2",
                o = out.display()
            ),
        );
        let input = dir.path().join("in.sql");
        std::fs::write(&input, "select 1;\n").unwrap();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let seen = progress.clone();
        let control = TransferControl::with_progress(Arc::new(false.into()), move |p| {
            seen.lock().unwrap().push((p.rows, p.bytes, p.total_bytes));
        });
        let outcome = run(&found, invocation(Some(input), Some("password=secret\n")), None, &control).await.unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text, "-rw-------\npassword=secret\narg one\nhunter22\nselect 1;\n");
        assert_eq!((outcome.objects, outcome.bytes), (1, 10));
        assert_eq!(outcome.warnings, ["from the plan", "warning: careful"]);
        assert_eq!(outcome.tool.as_deref(), Some("pg_dump 18.1"));
        let option_files = std::fs::read_dir(std::env::temp_dir()).unwrap().filter_map(Result::ok);
        assert!(option_files.filter(|e| e.file_name().to_string_lossy().starts_with("dataquery-")).all(|e| {
            // Other tests may run concurrently; ours must be gone.
            std::fs::read_to_string(e.path()).map_or(true, |c| c != "password=secret\n")
        }));

        let failing = script(
            dir.path(),
            "echo 'pg_dump: error: connection to server failed: FATAL:  password authentication failed for user \"u\" (pw hunter22)' >&2; exit 1",
        );
        let err = run(&failing, invocation(None, None), Some("hunter22"), &TransferControl::none()).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
        assert!(err.message.starts_with("pg_dump 18.1 failed (exit code 1): error: connection"), "{}", err.message);
        assert!(!err.message.contains("hunter22") && !err.detail.unwrap().contains("hunter22"), "password masked");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_the_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child-alive");
        // The tool starts a child of its own; both must end.
        let found = script(dir.path(), &format!("(sleep 2; touch {}) &\nsleep 30", marker.display()));
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control = TransferControl::new(cancelled.clone(), |_, _| {});
        let started = std::time::Instant::now();
        let flag = cancelled.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let err = run(&found, invocation(None, None), None, &control).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!marker.exists(), "the tool's own child was killed with it");
    }
}
