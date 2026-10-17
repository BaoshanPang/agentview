#![cfg(any(target_os = "linux", target_os = "macos"))]

use agentview::opencode_supervisor::OpenCodeSupervisor;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

struct Shutdown(Arc<OpenCodeSupervisor>);
impl Drop for Shutdown {
    fn drop(&mut self) {
        let _ = self.0.shutdown_server();
    }
}

struct Api {
    port: u16,
    password: String,
}

struct LocalProvider {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    port: u16,
}
impl LocalProvider {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut streams = Vec::new();
            while !signal.load(std::sync::atomic::Ordering::Relaxed) {
                if let Ok((mut stream, _)) = listener.accept() {
                    // Keep a local, token-free provider stream open until the
                    // real V2 session is explicitly interrupted. No tool calls.
                    let _=stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n");
                    streams.push(stream);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        Self {
            stop,
            thread: Some(thread),
            port,
        }
    }
}
impl Drop for LocalProvider {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Api {
    fn wait_running(&self, id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if self.call("GET", "/api/session/active", None)["data"]
                .get(id)
                .is_some()
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "local fixture session did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        // Credentials stay on curl's stdin, never in process argv.
        let mut config=format!("url = \"http://127.0.0.1:{}{}\"\nrequest = \"{}\"\nuser = \"opencode:{}\"\nheader = \"content-type: application/json\"\n",self.port,path,method,self.password);
        if let Some(body) = body {
            config.push_str(&format!(
                "data = {}\n",
                serde_json::to_string(&body.to_string()).unwrap()
            ));
        }
        let mut child = Command::new("curl")
            .args([
                "--silent",
                "--show-error",
                "--max-time",
                "10",
                "--config",
                "-",
                "--write-out",
                "\n%{http_code}",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(config.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let (body, status) = text.rsplit_once('\n').unwrap();
        assert!(status.starts_with('2'), "{method} {path}: {status} {body}");
        if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(body).unwrap()
        }
    }
    fn messages(&self, id: &str) -> Vec<Value> {
        self.call("GET", &format!("/api/session/{id}/message?limit=200"), None)["data"]
            .as_array()
            .unwrap()
            .clone()
    }
    fn continue_count(&self, id: &str) -> usize {
        self.messages(id)
            .iter()
            .filter(|m| m["type"] == "user" && m["text"] == "continue")
            .count()
    }
}

fn private_json(path: &Path, value: &Value) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
    file.sync_all().unwrap();
}

fn import_projection(
    api: &Api,
    template: &Value,
    id: &str,
    directory: &Path,
    completed: bool,
) -> Value {
    let mut info = template["data"]["info"].clone();
    info["id"] = json!(id);
    info["title"] = json!(format!("Migrated {id}"));
    info["agent"] = json!("build");
    info["model"] = json!({"providerID":"handoff-test","id":"unavailable"});
    info["location"] = json!({"directory":directory});
    info.as_object_mut().unwrap().remove("outcome");
    let start = info["time"]["created"].as_u64().unwrap();
    info["time"] = json!({"created":start,"updated":start+2});
    let mut assistant = json!({"id":format!("msg_{id}_assistant"),"type":"assistant","agent":"build","model":{"providerID":"handoff-test","id":"unavailable"},"content":[{"type":"text","text":"Tools settled before handoff"}],"time":{"created":start+1,"completed":start+2}});
    if completed {
        assistant["finish"] = json!("stop");
    } else {
        assistant["error"] =
            json!({"type":"aborted","message":"Source stopped at a tool-safe boundary"});
    }
    let messages = json!([
        {"id":format!("msg_{id}_user"),"type":"user","text":"Original frozen task","time":{"created":start}},
        assistant
    ]);
    api.call(
        "POST",
        "/api/experimental/session/import",
        Some(json!({"info":info,"messages":messages,"location":{"directory":directory}})),
    );
    api.call("GET", &format!("/api/session/{id}"), None)["data"].clone()
}

fn entry(api: &Api, session: &Value, resume: bool) -> Value {
    let id = session["id"].as_str().unwrap();
    let messages = api.messages(id);
    json!({"id":id,"title":session["title"],"directory":session["location"]["directory"],"agent":session["agent"],"model":session["model"],
        "lastMessageID":messages.first().map(|m|m["id"].clone()),"createdAtMs":session["time"]["created"],"updatedAtMs":session["time"]["updated"],
        "sourceState":if resume {"interrupted"} else {"completed"},"resume":resume,"toolsSettled":true,"externalActionsSettled":true})
}

fn cli(executable: &str, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_agentview"))
        .args(["--opencode-bin", executable, "opencode"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "handoff CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[ignore = "requires AGENTVIEW_REAL_OPENCODE_BIN; uses a fresh private V2 profile"]
fn real_v2_adopts_imported_sessions_and_reconciles_lost_ack_after_restart() {
    let executable = std::env::var("AGENTVIEW_REAL_OPENCODE_BIN").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for (key, dir) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("OPENCODE_CONFIG_DIR", "config/opencode"),
    ] {
        std::env::set_var(key, root.join(dir));
    }
    std::env::set_var("OPENCODE_DB", root.join("opencode.db"));
    std::env::set_var("AGENTVIEW_OPENCODE_API", "v2");
    std::env::remove_var("AGENTVIEW_OPENCODE_SERVICE_FILE");
    let provider = LocalProvider::start();
    fs::create_dir_all(root.join("config/opencode")).unwrap();
    private_json(
        &root.join("config/opencode/opencode.json"),
        &json!({"provider":{"handoff-test":{"npm":"@ai-sdk/openai-compatible","options":{"baseURL":format!("http://127.0.0.1:{}/v1",provider.port),"apiKey":"fixture"},"models":{"unavailable":{"name":"Local handoff fixture","limit":{"context":10000,"output":1000}}}}}}),
    );
    let bootstrap =
        Arc::new(OpenCodeSupervisor::with_state_dir(&executable, root.join("bootstrap")).unwrap());
    let _bootstrap_shutdown = Shutdown(bootstrap.clone());
    let seed = bootstrap
        .launch_with_model(
            "initialize isolated V2 fixture",
            root,
            Some("handoff-test/unavailable"),
        )
        .unwrap();
    let mut record: Value =
        serde_json::from_slice(&fs::read(root.join("bootstrap/server.json")).unwrap()).unwrap();
    let api = Api {
        port: record["port"].as_u64().unwrap() as u16,
        password: record["password"].as_str().unwrap().into(),
    };
    api.wait_running(&seed.id);
    bootstrap.interrupt(&seed.id).unwrap();
    api.call(
        "POST",
        &format!("/api/experimental/session/{}/wait", seed.id),
        None,
    );
    let template = api.call(
        "GET",
        &format!("/api/experimental/session/{}/export", seed.id),
        None,
    );
    let selected = import_projection(&api, &template, "ses_migrated_selected", root, false);
    let complete = import_projection(&api, &template, "ses_migrated_complete", root, true);
    let unselected = import_projection(&api, &template, "ses_migrated_unselected", root, false);
    let waiting = import_projection(&api, &template, "ses_migrated_waiting", root, false);
    let state = root.join("state/agentview/opencode");
    fs::create_dir_all(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    record["sessions"] = json!({});
    private_json(&state.join("server.json"), &record);
    let supervisor =
        Arc::new(OpenCodeSupervisor::with_state_dir(&executable, state.clone()).unwrap());
    let _shutdown = Shutdown(supervisor.clone());
    let endpoint = json!({"url":format!("http://127.0.0.1:{}",api.port),"pid":record["pid"]});
    let path = root.join("handoff.json");
    let mut selected_entry = entry(&api, &selected, true);
    let mut unselected_entry = entry(&api, &unselected, false);
    unselected_entry["sourceState"] = json!("interrupted");
    let manifest = json!({"version":1,"handoffID":"frozen-v1-cutover","endpoint":endpoint,"sessions":[selected_entry.clone(),entry(&api,&complete,false),unselected_entry]});

    // Full-batch validation must grant no partial ownership on any failure.
    let mut wrong = manifest.clone();
    wrong["endpoint"]["pid"] = json!(1);
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    assert!(supervisor.recorded_session_ids().unwrap().is_empty());
    wrong = manifest.clone();
    wrong["sessions"][0]["model"]["id"] = json!("wrong-model");
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    assert!(supervisor.recorded_session_ids().unwrap().is_empty());
    wrong = manifest.clone();
    wrong["sessions"][0]["externalActionsSettled"] = json!(false);
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    wrong = manifest.clone();
    let mut completed_entry = entry(&api, &complete, true);
    completed_entry["sourceState"] = json!("interrupted");
    wrong["sessions"] = json!([completed_entry]);
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    let form = api.call(
        "POST",
        "/api/session/ses_migrated_waiting/form",
        Some(json!({"title":"Awaiting decision","fields":[{"key":"answer","type":"string"}]})),
    );
    wrong = manifest.clone();
    wrong["sessions"] = json!([entry(&api, &waiting, true)]);
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    assert!(supervisor.recorded_session_ids().unwrap().is_empty());
    api.call(
        "DELETE",
        &format!(
            "/api/session/ses_migrated_waiting/form/{}",
            form["data"]["id"].as_str().unwrap()
        ),
        None,
    );
    api.call(
        "PATCH",
        "/api/session/ses_migrated_waiting",
        Some(json!({"permissions":[{"action":"*","resource":"*","effect":"ask"}]})),
    );
    let permission = api.call(
        "POST",
        "/api/session/ses_migrated_waiting/permission",
        Some(json!({"action":"handoff-check","resources":["/fixture/only"]})),
    );
    assert_eq!(permission["data"]["effect"], "ask");
    let waiting = api.call("GET", "/api/session/ses_migrated_waiting", None)["data"].clone();
    wrong = manifest.clone();
    wrong["sessions"] = json!([entry(&api, &waiting, true)]);
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    assert!(supervisor.recorded_session_ids().unwrap().is_empty());
    api.call(
        "POST",
        &format!(
            "/api/session/ses_migrated_waiting/permission/{}/reply",
            permission["data"]["id"].as_str().unwrap()
        ),
        Some(json!({"decision":"reject"})),
    );
    private_json(&path, &manifest);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(supervisor.import_handoff(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let imported = cli(&executable, &["handoff-import", path.to_str().unwrap()]);
    assert_eq!(imported["adopted"].as_array().unwrap().len(), 3);
    assert_eq!(supervisor.recorded_session_ids().unwrap().len(), 3);
    let visible = supervisor.list().unwrap();
    let row = visible
        .iter()
        .find(|s| s.id == "ses_migrated_selected")
        .unwrap();
    assert_eq!(row.title, selected["title"].as_str().unwrap());
    assert_eq!(row.summary, "Tools settled before handoff");
    assert_eq!(
        row.created_at_ms,
        selected["time"]["created"].as_u64().unwrap()
    );
    assert_eq!(
        api.continue_count("ses_migrated_selected"),
        0,
        "import must not resume"
    );
    let checkpoint = fs::read(state.join("handoffs.json")).unwrap();
    let ledger: Value = serde_json::from_slice(&checkpoint).unwrap();
    let input = ledger["handoffs"]["frozen-v1-cutover"]["roots"]["ses_migrated_selected"]
        ["inputId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(input.starts_with("msg_"));
    let late_form=api.call("POST","/api/session/ses_migrated_selected/form",Some(json!({"title":"Decision arrived after adoption","fields":[{"key":"answer","type":"string"}]})));
    assert_eq!(
        supervisor
            .list()
            .unwrap()
            .iter()
            .find(|s| s.id == "ses_migrated_selected")
            .unwrap()
            .state,
        agentview::domain::SessionState::NeedsInput
    );
    assert!(supervisor.resume_handoff("frozen-v1-cutover").is_err());
    assert_eq!(api.continue_count("ses_migrated_selected"), 0);
    api.call(
        "DELETE",
        &format!(
            "/api/session/ses_migrated_selected/form/{}",
            late_form["data"]["id"].as_str().unwrap()
        ),
        None,
    );
    let resumed = cli(&executable, &["handoff-resume", "frozen-v1-cutover"]);
    assert_eq!(resumed["resumed"], json!(["ses_migrated_selected"]));
    api.wait_running("ses_migrated_selected");
    assert_eq!(api.continue_count("ses_migrated_selected"), 1);
    assert_eq!(api.continue_count("ses_migrated_complete"), 0);
    assert_eq!(api.continue_count("ses_migrated_unselected"), 0);
    assert!(api
        .messages("ses_migrated_selected")
        .iter()
        .any(|m| m["id"] == input));

    // Simulate admission succeeding but its HTTP ACK/local receipt being lost:
    // restore the exact durable checkpoint from immediately before that POST.
    fs::write(state.join("handoffs.json"), checkpoint).unwrap();
    let restarted = supervisor.restart_server().unwrap();
    assert!(restarted
        .awaiting_input
        .contains(&"ses_migrated_selected".to_owned()));
    assert!(
        !restarted
            .resumed
            .contains(&"ses_migrated_selected".to_owned()),
        "restart must not add an anonymous second continuation"
    );
    let new_record: Value =
        serde_json::from_slice(&fs::read(state.join("server.json")).unwrap()).unwrap();
    let api = Api {
        port: new_record["port"].as_u64().unwrap() as u16,
        password: new_record["password"].as_str().unwrap().into(),
    };
    cli(&executable, &["handoff-import", path.to_str().unwrap()]);
    let retried = cli(&executable, &["handoff-resume", "frozen-v1-cutover"]);
    assert_eq!(retried["alreadyAccepted"], json!(["ses_migrated_selected"]));
    assert_eq!(
        api.continue_count("ses_migrated_selected"),
        1,
        "lost ACK/restart must reuse original input"
    );
    let again = supervisor.resume_handoff("frozen-v1-cutover").unwrap();
    assert_eq!(again.already_accepted, vec!["ses_migrated_selected"]);
    assert_eq!(api.continue_count("ses_migrated_unselected"), 0);

    // Adoption grants real managed controls to existing imported IDs.
    supervisor
        .reply("ses_migrated_complete", "new work after adoption")
        .unwrap();
    api.wait_running("ses_migrated_complete");
    supervisor.interrupt("ses_migrated_complete").unwrap();
    api.call(
        "POST",
        "/api/experimental/session/ses_migrated_complete/wait",
        None,
    );
    assert!(api
        .messages("ses_migrated_complete")
        .iter()
        .any(|m| m["type"] == "user" && m["text"] == "new work after adoption"));
    let command = supervisor
        .native_attach_command("ses_migrated_complete")
        .unwrap();
    assert!(command.get_args().any(|a| a == "ses_migrated_complete"));
    // Changing a previously imported manifest cannot broaden resume authority.
    selected_entry["resume"] = json!(false);
    wrong = manifest.clone();
    wrong["sessions"][0] = selected_entry;
    private_json(&path, &wrong);
    assert!(supervisor.import_handoff(&path).is_err());
    supervisor.shutdown_server().unwrap();
    assert!(supervisor.live_server_pid().unwrap().is_none());
}
