#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use agentview::adapters::{
    DiscoveryEngine, DiscoveryRequest, FixtureSource, OpenCodeSource, PiSource, SshHostSource,
};
use agentview::domain::Provider;
use tempfile::tempdir;

const PI_SESSION: &str = r#"{"type":"session","version":3,"id":"pi-isolated","timestamp":"2026-08-18T12:00:00Z","cwd":"/work/pi"}
{"type":"message","id":"a1","parentId":null,"timestamp":"2026-08-18T12:00:01Z","message":{"role":"user","content":"Pi task"}}
{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-08-18T12:00:02Z","message":{"role":"assistant","content":[{"type":"text","text":"Pi done"}],"stopReason":"stop"}}
"#;

fn open_code_script() -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let executable = directory.path().join("opencode");
    fs::write(
        &executable,
        r#"#!/bin/sh
if [ "$1" = session ] && [ "$2" = list ] && [ "$3" = --format ] && [ "$4" = json ]; then
  printf '%s\n' '[{"id":"oc-isolated","title":"OpenCode task","updated":1787089210008,"created":1787089195916,"projectId":"global","directory":"/work/opencode"}]'
  exit 0
fi
exit 64
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).unwrap();
    (directory, executable)
}

fn pi_store() -> tempfile::TempDir {
    let directory = tempdir().unwrap();
    let nested = directory.path().join("--work-pi--");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("session.jsonl"), PI_SESSION).unwrap();
    directory
}

/// A stand-in `ssh` executable that answers like a passwordless host whose
/// agentview reports one managed OpenCode session.
fn ssh_script() -> (tempfile::TempDir, PathBuf) {
    let directory = tempdir().unwrap();
    let executable = directory.path().join("ssh");
    fs::write(
        &executable,
        r#"#!/bin/sh
for argument in "$@"; do
  case "$argument" in
    176)
      host=176
      ;;
    AGENTVIEW_REMOTE=1*)
      ;;
    *)
      remote="$argument"
      ;;
  esac
done
[ "$host" = 176 ] || exit 66
[ "${remote#*AGENTVIEW_REMOTE=1}" != "$remote" ] || exit 67
printf '%s\n' "$remote" >&2
cat <<'JSON'
{
  "sessions": [
    {
      "id": "opencode:host:ses_remote",
      "provider_session_id": "ses_remote",
      "provider": "opencode",
      "runtime": {"kind": "host"},
      "kind": "managed",
      "name": "Remote build",
      "cwd": "/home/bpang/mywork",
      "state": "working",
      "summary": "Compiling",
      "raw_state": "managed_server",
      "pid": 496692,
      "started_at": 1793134692584,
      "updated_at": 1793134809515,
      "pull_requests": null,
      "capabilities": ["inspect", "reply", "interrupt"]
    }
  ],
  "warnings": []
}
JSON
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).unwrap();
    (directory, executable)
}

#[test]
fn ssh_host_merges_remote_agentview_sessions_as_inspect_only() {
    let (_directory, ssh) = ssh_script();
    let mut engine = DiscoveryEngine::new();
    engine.add_source(SshHostSource::with_ssh_bin("176", ssh.display().to_string()));

    let snapshot = engine.discover(&DiscoveryRequest::default());

    assert!(snapshot.warnings.is_empty(), "{:?}", snapshot.warnings);
    assert_eq!(snapshot.sessions.len(), 1);
    let session = &snapshot.sessions[0];
    assert_eq!(session.id, "ssh:176:opencode:host:ses_remote");
    assert_eq!(session.provider, Provider::OpenCode);
    assert_eq!(
        session.runtime,
        agentview::domain::Runtime::Ssh {
            host: "176".into()
        }
    );
    assert_eq!(session.pid, None);
    assert_eq!(session.capabilities.len(), 1);
    assert!(session
        .capabilities
        .contains(&agentview::domain::Capability::Inspect));
}

#[test]
fn pi_discovers_in_isolation() {
    let store = pi_store();
    let mut engine = DiscoveryEngine::new();
    engine.add_source(PiSource::host(store.path()));

    let snapshot = engine.discover(&DiscoveryRequest {
        include_completed: true,
        include_external: true,
        ..DiscoveryRequest::default()
    });

    assert!(snapshot.warnings.is_empty());
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].provider, Provider::Pi);
}

#[test]
fn opencode_discovers_in_isolation() {
    let (_directory, executable) = open_code_script();
    let mut engine = DiscoveryEngine::new();
    engine.add_source(OpenCodeSource::host(executable.display().to_string()));

    let snapshot = engine.discover(&DiscoveryRequest {
        include_completed: true,
        include_external: true,
        ..DiscoveryRequest::default()
    });

    assert!(snapshot.warnings.is_empty());
    assert_eq!(snapshot.sessions.len(), 1);
    assert_eq!(snapshot.sessions[0].provider, Provider::OpenCode);
}

#[test]
fn pi_opencode_claude_and_codex_coexist_without_collisions() {
    let store = pi_store();
    let (_directory, executable) = open_code_script();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/populated-sessions.json");
    let mut engine = DiscoveryEngine::new();
    engine.add_source(PiSource::host(store.path()));
    engine.add_source(OpenCodeSource::host(executable.display().to_string()));
    engine.add_source(FixtureSource::new(fixture));

    let snapshot = engine.discover(&DiscoveryRequest {
        include_completed: true,
        include_interactive: true,
        include_external: true,
        cwd: None,
        ..DiscoveryRequest::default()
    });

    assert!(snapshot.warnings.is_empty());
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Pi,
        Provider::OpenCode,
    ] {
        assert!(snapshot
            .sessions
            .iter()
            .any(|session| session.provider == provider));
    }
    let mut ids = snapshot
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), snapshot.sessions.len());
}

#[test]
fn canonical_visual_fixture_contains_every_supported_provider_without_id_collisions() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/all-providers-sessions.json");
    let mut engine = DiscoveryEngine::new();
    engine.add_source(FixtureSource::new(fixture));

    let snapshot = engine.discover(&DiscoveryRequest {
        include_completed: true,
        include_interactive: true,
        cwd: None,
        ..DiscoveryRequest::default()
    });

    assert!(snapshot.warnings.is_empty());
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Pi,
        Provider::OpenCode,
        Provider::Cursor,
        Provider::GitHubCopilot,
        Provider::Antigravity,
        Provider::MistralVibe,
        Provider::MuseCode,
        Provider::QwenCode,
        Provider::KimiCode,
        Provider::OhMyPi,
        Provider::Grok,
        Provider::KiloCode,
        Provider::OpenHands,
        Provider::Terminal,
    ] {
        assert!(
            snapshot
                .sessions
                .iter()
                .any(|session| session.provider == provider),
            "missing {} fixture session",
            provider.label()
        );
    }
    let mut ids = snapshot
        .sessions
        .iter()
        .map(|session| session.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), snapshot.sessions.len());
}
