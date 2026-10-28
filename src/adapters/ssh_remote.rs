//! Sessions on passwordless SSH hosts that run agentview themselves.
//!
//! One refresh runs `agentview --json` on the remote host over SSH and merges
//! the returned snapshot. Remote rows are observation-first: their
//! capabilities are reduced to `Inspect`, their process IDs are cleared, and
//! their runtime becomes [`Runtime::Ssh`]. Opening one is routed by
//! `ControlHub::open` to the provider's native attach over SSH; inline reply
//! and launch stay local-only. The remote agentview receives
//! `AGENTVIEW_REMOTE=1` so a host that is also a target never fans out
//! recursively.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::{DiscoveryRequest, SessionSource, SourceDiscovery};
use crate::domain::{AgentSession, Capability, Runtime};
use crate::fs_util;
use crate::paused::{ensure_private_directory, RegistryLock};
use crate::process::{CancellableProcessRunner, CommandRequest, CommandRunner};

/// Set in the remote command so the remote agentview ignores its own SSH
/// targets: a machine that both runs sessions and watches others must not
/// observe itself.
pub const REMOTE_GUARD_ENV: &str = "AGENTVIEW_REMOTE";

/// A remote command runs through a non-interactive shell whose PATH misses the
/// per-user agent CLI homes (OpenCode installs into `~/.opencode/bin`, npm
/// globals into `~/.npm-global/bin`). Discovery and remote open both export
/// this so the same binaries are reachable in both directions, and child
/// processes inherit it.
pub const REMOTE_PATH_PREFIX: &str =
    "export PATH=\"$HOME/.local/bin:$HOME/.opencode/bin:$HOME/.npm-global/bin:$HOME/.bun/bin:$HOME/bin:$PATH\"";

const REGISTRY_VERSION: u32 = 1;
/// A full remote discovery runs every provider on that host.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(45);

pub struct SshHostSource {
    label: String,
    host: String,
    ssh_bin: String,
    runner: Arc<dyn CommandRunner>,
}

impl SshHostSource {
    pub fn new(host: impl Into<String>) -> Self {
        Self::with_ssh_bin(host, "ssh")
    }

    pub fn with_ssh_bin(host: impl Into<String>, ssh_bin: impl Into<String>) -> Self {
        Self::with_runner_and_bin(host, ssh_bin, Arc::new(CancellableProcessRunner::default()))
    }

    pub fn with_runner_and_bin(
        host: impl Into<String>,
        ssh_bin: impl Into<String>,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        let host = host.into();
        Self {
            label: format!("{host} (ssh)"),
            host,
            ssh_bin: ssh_bin.into(),
            runner,
        }
    }

    #[cfg(test)]
    fn with_runner(host: impl Into<String>, runner: Arc<dyn CommandRunner>) -> Self {
        Self::with_runner_and_bin(host, "ssh", runner)
    }

    fn remote_command(&self) -> String {
        format!(
            "{REMOTE_PATH_PREFIX}; export {REMOTE_GUARD_ENV}=1; agentview \
             --json --include-interactive --include-external"
        )
    }

    fn ssh_request(&self) -> CommandRequest {
        let mut command = CommandRequest::new(
            self.ssh_bin.clone(),
            vec![
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=10".into(),
                self.host.clone(),
                self.remote_command(),
            ],
        );
        command.timeout = DISCOVERY_TIMEOUT;
        command
    }

    pub(crate) fn discover_remote(&self, _request: &DiscoveryRequest) -> Result<SourceDiscovery> {
        let output = self.runner.run(&self.ssh_request())?;
        if output.status != 0 {
            bail!(
                "agentview on {host} exited with status {status}: {stderr}",
                host = self.host,
                status = output.status,
                stderr = output.stderr_lossy()
            );
        }
        let (mut sessions, mut warnings) = parse_remote_snapshot(&output.stdout)?;
        for session in &mut sessions {
            remap_row(session, &self.host);
        }
        let prefix = format!("{}: ", self.host);
        for warning in &mut warnings {
            *warning = format!("{prefix}{warning}");
        }
        Ok(SourceDiscovery {
            sessions,
            warnings,
            kept: Default::default(),
        })
    }
}

impl SessionSource for SshHostSource {
    fn label(&self) -> &str {
        &self.label
    }

    fn discover(&self, request: &DiscoveryRequest) -> Result<Vec<AgentSession>> {
        Ok(self.discover_remote(request)?.sessions)
    }

    fn discover_with_warnings(&self, request: &DiscoveryRequest) -> Result<SourceDiscovery> {
        self.discover_remote(request)
    }

    fn cancel(&self) {
        self.runner.cancel();
    }
}

/// The SSH hosts this dashboard watches, shared between the discovery source
/// and the dashboard so `/remote add` and `/remote remove` take effect on the
/// next refresh. `frozen` hosts refuse changes: a machine that is itself being
/// observed (`AGENTVIEW_REMOTE=1`) observes nothing. `save_path` persists
/// every change; `None` keeps tests and fixtures from touching state.
#[derive(Clone, Debug)]
pub struct SshTargetList {
    hosts: Arc<Mutex<BTreeSet<String>>>,
    frozen: bool,
    save_path: Option<PathBuf>,
}

impl SshTargetList {
    /// A list that refuses changes and watches nothing.
    pub fn disabled() -> Self {
        Self {
            hosts: Arc::new(Mutex::new(BTreeSet::new())),
            frozen: true,
            save_path: None,
        }
    }

    pub fn enabled(hosts: Vec<String>, save_path: PathBuf) -> Self {
        Self {
            hosts: Arc::new(Mutex::new(hosts.into_iter().collect())),
            frozen: false,
            save_path: Some(save_path),
        }
    }

    pub fn hosts(&self) -> BTreeSet<String> {
        self.hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Watch one more host. Returns whether the list changed.
    pub fn insert(&self, host: &str) -> Result<bool> {
        if self.frozen {
            bail!("remote hosts are disabled while agentview runs as a watched host");
        }
        validate_host(host)?;
        let changed = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(host.to_owned());
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    /// Stop watching one host. Returns whether the list changed.
    pub fn remove(&self, host: &str) -> Result<bool> {
        if self.frozen {
            bail!("remote hosts are disabled while agentview runs as a watched host");
        }
        let changed = self
            .hosts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(host);
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    fn persist(&self) -> Result<()> {
        match &self.save_path {
            Some(path) => {
                let hosts = self.hosts();
                save_ssh_targets_at(path, &hosts)
            }
            None => Ok(()),
        }
    }
}

/// One dashboard source for every watched SSH host. The host list is read on
/// each refresh, so `/remote add` and `/remote remove` change what the next
/// refresh reports without restarting agentview. Runners stay per host so an
/// in-flight SSH discovery can still be cancelled on exit.
pub struct SshHostsSource {
    label: String,
    hosts: SshTargetList,
    ssh_bin: String,
    runners: Mutex<BTreeMap<String, Arc<dyn CommandRunner>>>,
}

impl SshHostsSource {
    pub fn new(hosts: SshTargetList, ssh_bin: impl Into<String>) -> Self {
        Self {
            label: "SSH hosts".into(),
            hosts,
            ssh_bin: ssh_bin.into(),
            runners: Mutex::new(BTreeMap::new()),
        }
    }

    fn host_source(&self, host: &str) -> SshHostSource {
        let runner = self
            .runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(host.to_owned())
            .or_insert_with(|| Arc::new(CancellableProcessRunner::default()) as Arc<dyn CommandRunner>)
            .clone();
        SshHostSource::with_runner_and_bin(host, &self.ssh_bin, runner)
    }
}

impl SessionSource for SshHostsSource {
    fn label(&self) -> &str {
        &self.label
    }

    fn discover(&self, request: &DiscoveryRequest) -> Result<Vec<AgentSession>> {
        Ok(self.discover_with_warnings(request)?.sessions)
    }

    fn discover_with_warnings(&self, request: &DiscoveryRequest) -> Result<SourceDiscovery> {
        let hosts = self.hosts.hosts();
        self.runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|host, _| hosts.contains(host));
        if hosts.is_empty() {
            return Ok(SourceDiscovery::default());
        }
        let sources = hosts
            .iter()
            .map(|host| self.host_source(host))
            .collect::<Vec<_>>();
        aggregate_host_discoveries(&sources, request)
    }

    fn cancel(&self) {
        for runner in self
            .runners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
        {
            runner.cancel();
        }
    }
}

/// Discover every host concurrently and merge the results. Each host keeps its
/// own completed-history budget, exactly as one-source-per-host did, so adding
/// a host never steals another host's completed rows.
fn aggregate_host_discoveries(
    sources: &[SshHostSource],
    request: &DiscoveryRequest,
) -> Result<SourceDiscovery> {
    let mut merged = SourceDiscovery::default();
    std::thread::scope(|scope| {
        let results = sources
            .iter()
            .map(|source| {
                scope.spawn(move || {
                    let result = source.discover_remote(request);
                    (source.label().to_owned(), result)
                })
            })
            .collect::<Vec<_>>();
        for worker in results {
            let (label, result) = worker.join().unwrap_or_else(|_| {
                (
                    "SSH host".to_owned(),
                    Err(anyhow::anyhow!("provider discovery panicked")),
                )
            });
            match result {
                Ok(mut discovered) => {
                    let kept = std::mem::take(&mut discovered.kept);
                    if super::bound_completed_history(
                        &mut discovered.sessions,
                        &kept,
                        request,
                    ) {
                        discovered.warnings.push(format!(
                            "{label} completed history is limited to {} records for this refresh; increase --history-limit to load more",
                            request.history_limit.max(1)
                        ));
                    }
                    // The rows that survive each host's own budget are listed
                    // on purpose, so the engine-level budget over the merged
                    // list never drops one host's rows for another host's.
                    let mut kept = kept;
                    for session in &discovered.sessions {
                        if session.state == crate::domain::SessionState::Completed {
                            kept.insert(session.id.clone());
                        }
                    }
                    merged.sessions.append(&mut discovered.sessions);
                    merged.warnings.append(&mut discovered.warnings);
                    merged.kept.extend(kept);
                }
                Err(error) => merged
                    .warnings
                    .push(format!("{label}: {error:#}")),
            }
        }
    });
    Ok(merged)
}

/// Reduce one remote row to a read-only observation of that host.
fn remap_row(session: &mut AgentSession, host: &str) {
    session.id = format!("ssh:{host}:{}", session.id);
    session.runtime = match &session.runtime {
        Runtime::Docker { container_name, .. } => Runtime::Ssh {
            host: format!("{host}/{container_name}"),
        },
        _ => Runtime::Ssh {
            host: host.to_owned(),
        },
    };
    session.pid = None;
    session.capabilities = BTreeSet::from([Capability::Inspect]);
}

/// Parse the remote `agentview --json` output. Shell banners before or after
/// the document are ignored, and sessions a newer agentview added remain
/// parseable one by one.
fn parse_remote_snapshot(stdout: &[u8]) -> Result<(Vec<AgentSession>, Vec<String>)> {
    let text = String::from_utf8_lossy(stdout);
    let start = text
        .find('{')
        .context("agentview on the remote host printed no JSON")?;
    let end = text
        .rfind('}')
        .context("agentview on the remote host printed an incomplete JSON document")?;
    if start >= end {
        bail!("agentview on the remote host printed an unexpected JSON shape");
    }
    let document = &text[start..=end];
    if let Ok(snapshot) = serde_json::from_str::<crate::domain::SessionSnapshot>(document) {
        return Ok((snapshot.sessions, snapshot.warnings));
    }
    let value: serde_json::Value = serde_json::from_str(document)
        .context("invalid agentview --json output from the remote host")?;
    let mut sessions = Vec::new();
    let mut skipped = 0_usize;
    for record in value
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        match serde_json::from_value::<AgentSession>(record.clone()) {
            Ok(session) => sessions.push(session),
            Err(_) => skipped += 1,
        }
    }
    let mut warnings = value
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|warning| warning.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    if skipped > 0 {
        warnings.push(format!(
            "{skipped} sessions were skipped because this agentview does not know their format"
        ));
    }
    Ok((sessions, warnings))
}

/// SSH hosts this installation watches, persisted so awareness survives
/// restarts without repeating `--ssh-host` on every launch.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SshTargets {
    pub hosts: BTreeSet<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct SshTargetsDocument {
    version: u32,
    hosts: Vec<String>,
}

pub fn default_ssh_targets_path() -> Result<PathBuf> {
    if let Some(state_home) = crate::fs_util::xdg_home("XDG_STATE_HOME") {
        return Ok(PathBuf::from(state_home)
            .join("agentview")
            .join("ssh-targets.json"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/agentview/ssh-targets.json"))
}

/// Hosts from the state file. A missing file means no targets.
pub fn load_ssh_targets() -> Result<SshTargets> {
    load_ssh_targets_at(&default_ssh_targets_path()?)
}

pub fn load_ssh_targets_at(path: &std::path::Path) -> Result<SshTargets> {
    let Ok(input) = fs::read_to_string(path) else {
        return Ok(SshTargets::default());
    };
    let document: SshTargetsDocument = serde_json::from_str(&input)
        .with_context(|| format!("invalid SSH targets registry {}", path.display()))?;
    if document.version != REGISTRY_VERSION {
        bail!(
            "unsupported SSH targets registry version {} in {}",
            document.version,
            path.display()
        );
    }
    let mut hosts = BTreeSet::new();
    for host in document.hosts {
        validate_host(&host).with_context(|| format!("invalid SSH target {}", path.display()))?;
        hosts.insert(host);
    }
    Ok(SshTargets { hosts })
}

/// Persist the merged host set. Explicit `--ssh-host` flags make an agentview
/// aware of a host for every future run.
pub fn save_ssh_targets(hosts: &BTreeSet<String>) -> Result<()> {
    save_ssh_targets_at(&default_ssh_targets_path()?, hosts)
}

pub fn save_ssh_targets_at(
    path: &std::path::Path,
    hosts: &BTreeSet<String>,
) -> Result<()> {
    for host in hosts {
        validate_host(host)?;
    }
    if let Some(parent) = path.parent() {
        ensure_private_directory(parent)?;
    }
    let _lock = RegistryLock::acquire(&path.with_file_name("ssh-targets.lock"))?;
    let document = SshTargetsDocument {
        version: REGISTRY_VERSION,
        hosts: hosts.iter().cloned().collect(),
    };
    fs_util::write_private_json(path, &document)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Only host aliases and plain hostnames are accepted: each part becomes an
/// SSH argument, and anything that could act as a flag or contain shell
/// separation is rejected.
pub fn validate_host(host: &str) -> Result<()> {
    let first = host
        .chars()
        .next()
        .context("SSH target must not be empty")?;
    if first == '-' {
        bail!("SSH target must not start with '-'");
    }
    if host.chars().any(|character| {
        character.is_whitespace() || character == ';' || character == '$' || character == '\''
    }) {
        bail!("SSH target contains unsupported characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::CommandOutput;
    use std::sync::Mutex;

    struct FakeRunner {
        expected: CommandRequest,
        output: Mutex<Option<CommandOutput>>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput> {
            assert_eq!(request, &self.expected);
            Ok(self.output.lock().unwrap().take().unwrap())
        }
    }

    fn snapshot_fixture() -> String {
        serde_json::json!({
            "sessions": [
                {
                    "id": "opencode:host:ses_one",
                    "provider_session_id": "ses_one",
                    "provider": "opencode",
                    "runtime": {"kind": "host"},
                    "kind": "managed",
                    "name": "Fix the parser",
                    "cwd": "/home/bpang/mywork",
                    "state": "working",
                    "summary": "Rewriting the lexer",
                    "raw_state": "managed_server",
                    "pid": 496692,
                    "started_at": 1793134692584_u64,
                    "updated_at": 1793134809515_u64,
                    "pull_requests": null,
                    "capabilities": ["inspect", "reply", "interrupt"]
                },
                {
                    "id": "claude:host:11111111-2222-3333-4444-555555555555",
                    "provider_session_id": "11111111-2222-3333-4444-555555555555",
                    "provider": "claude",
                    "runtime": {"kind": "docker", "container_id": "abc123",
                        "container_name": "dev", "image": "ubuntu:22.04"},
                    "kind": "background",
                    "name": "claude-1111111",
                    "cwd": "/srv/work",
                    "state": "completed",
                    "summary": "finished",
                    "raw_state": null,
                    "pid": 42,
                    "started_at": null,
                    "updated_at": 1793134700000_u64,
                    "pull_requests": null,
                    "capabilities": ["inspect"]
                }
            ],
            "warnings": ["Docker: permission denied"]
        })
        .to_string()
    }

    fn source_with(output: String) -> SshHostSource {
        let expected = CommandRequest {
            program: "ssh".into(),
            args: vec![
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ConnectTimeout=10".into(),
                "176".into(),
                format!(
                    "{REMOTE_PATH_PREFIX}; export AGENTVIEW_REMOTE=1; agentview --json --include-interactive --include-external"
                ),
            ],
            current_dir: None,
            timeout: DISCOVERY_TIMEOUT,
            stdout_file: None,
        };
        SshHostSource::with_runner(
            "176",
            Arc::new(FakeRunner {
                expected,
                output: Mutex::new(Some(CommandOutput {
                    status: 0,
                    stdout: output.into_bytes(),
                    stderr: Vec::new(),
                })),
            }),
        )
    }

    #[test]
    fn discovery_merges_remote_rows_as_inspect_only_ssh_sessions() {
        let source = source_with(snapshot_fixture());
        let discovered = source
            .discover_with_warnings(&DiscoveryRequest::default())
            .unwrap();

        let rows = &discovered.sessions;
        assert_eq!(rows.len(), 2);
        let opencode = &rows[0];
        assert_eq!(opencode.id, "ssh:176:opencode:host:ses_one");
        assert_eq!(
            opencode.runtime,
            Runtime::Ssh {
                host: "176".into()
            }
        );
        assert_eq!(opencode.pid, None);
        assert_eq!(
            opencode.capabilities,
            BTreeSet::from([Capability::Inspect])
        );
        assert_eq!(opencode.name, "Fix the parser");
        assert_eq!(opencode.state, crate::domain::SessionState::Working);

        let claude = &rows[1];
        assert_eq!(claude.id, "ssh:176:claude:host:11111111-2222-3333-4444-555555555555");
        assert_eq!(
            claude.runtime,
            Runtime::Ssh {
                host: "176/dev".into()
            }
        );

        assert_eq!(
            discovered.warnings,
            vec!["176: Docker: permission denied"]
        );
    }

    #[test]
    fn banner_noise_around_the_document_is_ignored() {
        let output = format!(
            "Fortinet authorized uses only.\n{}\nAgentview out.\n",
            snapshot_fixture()
        );
        let source = source_with(output);
        let discovered = source
            .discover_with_warnings(&DiscoveryRequest::default())
            .unwrap();
        assert_eq!(discovered.sessions.len(), 2);
    }

    #[test]
    fn a_failed_remote_run_reports_the_stderr() {
        let failing = {
            let mut source = SshHostSource::new("176");
            let expected = source.ssh_request();
            struct Failing {
                expected: CommandRequest,
            }
            impl CommandRunner for Failing {
                fn run(&self, request: &CommandRequest) -> Result<CommandOutput> {
                    assert_eq!(request, &self.expected);
                    Ok(CommandOutput {
                        status: 255,
                        stdout: Vec::new(),
                        stderr: b"ssh: Could not resolve hostname nope".to_vec(),
                    })
                }
            }
            source.runner = Arc::new(Failing { expected });
            source
        };
        let error = failing.discover(&DiscoveryRequest::default()).unwrap_err();
        assert!(
            error.to_string().contains("status 255"),
            "unexpected error: {error:#}"
        );

        // A zero status with no JSON still fails with a clear message.
        let error = source_with(String::new())
            .discover(&DiscoveryRequest::default())
            .unwrap_err();
        assert!(
            error.to_string().contains("printed no JSON"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn unknown_future_sessions_are_skipped_with_a_warning() {
        let output = r#"{
            "sessions": [
                {"id": "opencode:host:ses_old", "provider_session_id": "ses_old",
                 "provider": "opencode", "runtime": {"kind": "host"}, "kind": "managed",
                 "name": "old", "cwd": "/w", "state": "working", "summary": "", "raw_state": null,
                 "pid": null, "started_at": null, "updated_at": null, "pull_requests": null,
                 "capabilities": []},
                {"id": "future:host:ses_new", "provider_session_id": "ses_new",
                 "provider": "future", "runtime": {"kind": "host"}, "kind": "managed",
                 "name": "new", "cwd": "/w", "state": "working", "summary": "", "raw_state": null,
                 "pid": null, "started_at": null, "updated_at": null, "pull_requests": null,
                 "capabilities": [], "brandNewField": true}
            ],
            "warnings": []
        }"#
        .to_string();
        let source = source_with(output);
        let discovered = source
            .discover_with_warnings(&DiscoveryRequest::default())
            .unwrap();
        assert_eq!(discovered.sessions.len(), 1);
        assert_eq!(
            discovered.warnings,
            vec!["176: 1 sessions were skipped because this agentview does not know their format"]
        );
    }

    #[test]
    fn ssh_targets_round_trip_and_validate() {
        validate_host("176").unwrap();
        validate_host("bpang@172.30.80.176").unwrap();
        validate_host("-oFoo=bar").unwrap_err();
        validate_host("17 6").unwrap_err();
        validate_host("a;b").unwrap_err();

        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = directory.path().join("ssh-targets.json");
        assert!(load_ssh_targets_at(&path).unwrap().hosts.is_empty());
        let mut hosts = BTreeSet::new();
        hosts.insert("176".into());
        hosts.insert("177".into());
        save_ssh_targets_at(&path, &hosts).unwrap();
        assert_eq!(load_ssh_targets_at(&path).unwrap().hosts, hosts);

        let corrupt = directory.path().join("corrupt.json");
        fs::write(&corrupt, "{\"version\": 9, \"hosts\": []}").unwrap();
        assert!(load_ssh_targets_at(&corrupt).is_err());
    }

    #[test]
    fn ssh_target_list_persists_changes_and_refuses_when_frozen() {
        validate_host("-oProxy=evil").unwrap_err();
        assert!(SshTargetList::disabled()
            .insert("176")
            .unwrap_err()
            .to_string()
            .contains("remote hosts are disabled"));

        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let registry = directory.path().join("ssh-targets.json");
        let list = SshTargetList::enabled(Vec::new(), registry.clone());

        assert!(list.insert("176").unwrap());
        assert_eq!(list.hosts(), ["176".to_owned()].into_iter().collect());
        assert_eq!(
            load_ssh_targets_at(&registry).unwrap().hosts,
            list.hosts()
        );
        assert!(!list.insert("176").unwrap());
        assert!(list.insert("bad host").is_err());
        assert!(!list.remove("177").unwrap());
        assert!(list.remove("176").unwrap());
        assert!(list.hosts().is_empty());
        assert!(load_ssh_targets_at(&registry).unwrap().hosts.is_empty());
    }

    #[test]
    fn ssh_hosts_source_reports_nothing_when_no_host_is_watched() {
        let source = SshHostsSource::new(SshTargetList::disabled(), "ssh");
        let discovered = source
            .discover_with_warnings(&DiscoveryRequest::default())
            .unwrap();
        assert!(discovered.sessions.is_empty());
        assert!(discovered.warnings.is_empty());
    }

    fn completed_fixture(host: &str, count: usize) -> String {
        let sessions = (0..count)
            .map(|index| {
                serde_json::json!({
                    "id": format!("opencode:host:ses_{host}_{index}"),
                    "provider_session_id": format!("ses_{host}_{index}"),
                    "provider": "opencode",
                    "runtime": {"kind": "host"},
                    "kind": "managed",
                    "name": format!("{host} finished {index}"),
                    "cwd": "/home/bpang/mywork",
                    "state": if index == 0 { "working" } else { "completed" },
                    "summary": "done",
                    "raw_state": "managed_server",
                    "pid": 1000 + index,
                    "started_at": 1793134692584_u64,
                    "updated_at": 1793134809515_u64 + index as u64,
                    "pull_requests": null,
                    "capabilities": ["inspect"]
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({ "sessions": sessions, "warnings": [] }).to_string()
    }

    fn fake_host(host: &str, output: String) -> SshHostSource {
        let expected = SshHostSource::new(host).ssh_request();
        SshHostSource::with_runner(
            host,
            Arc::new(FakeRunner {
                expected,
                output: Mutex::new(Some(CommandOutput {
                    status: 0,
                    stdout: output.into_bytes(),
                    stderr: Vec::new(),
                })),
            }),
        )
    }

    #[test]
    fn aggregation_keeps_each_host_completed_budget_and_prefixes_warnings() {
        let request = DiscoveryRequest {
            include_completed: true,
            history_limit: 2,
            ..DiscoveryRequest::default()
        };
        // Each host reports one working row plus three completed rows, so the
        // per-host budget keeps the two newest completed rows per host.
        let sources = vec![
            fake_host("176", completed_fixture("a", 4)),
            fake_host("177", completed_fixture("b", 4)),
        ];
        let merged = aggregate_host_discoveries(&sources, &request).unwrap();

        assert_eq!(merged.sessions.len(), 6);
        let completed = merged
            .sessions
            .iter()
            .filter(|session| session.state == crate::domain::SessionState::Completed)
            .count();
        assert_eq!(completed, 4);
        assert_eq!(merged.kept.len(), 4);
        assert_eq!(merged.warnings.len(), 2);
        assert!(merged.warnings[0].starts_with("176 (ssh) completed history is limited"));
        assert!(merged.warnings[1].starts_with("177 (ssh) completed history is limited"));
    }

    #[test]
    fn aggregation_reports_one_unreachable_host_without_losing_the_others() {
        let request = DiscoveryRequest {
            include_completed: true,
            ..DiscoveryRequest::default()
        };
        let failing = {
            let mut source = SshHostSource::new("177");
            let expected = source.ssh_request();
            struct Failing {
                expected: CommandRequest,
            }
            impl CommandRunner for Failing {
                fn run(&self, request: &CommandRequest) -> Result<CommandOutput> {
                    assert_eq!(request, &self.expected);
                    Ok(CommandOutput {
                        status: 255,
                        stdout: Vec::new(),
                        stderr: b"ssh: connect refused".to_vec(),
                    })
                }
            }
            SshHostSource::with_runner("177", Arc::new(Failing { expected }))
        };
        let sources = vec![fake_host("176", completed_fixture("a", 2)), failing];
        let merged = aggregate_host_discoveries(&sources, &request).unwrap();
        assert_eq!(merged.sessions.len(), 2);
        assert!(merged.warnings.len() == 1 && merged.warnings[0].starts_with("177 (ssh): "));
    }
}
