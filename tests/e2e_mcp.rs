use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

fn eyra_bin() -> &'static str {
    env!("CARGO_BIN_EXE_eyra")
}

struct TestGuiApp {
    child: Child,
    script_path: PathBuf,
    title: String,
}

impl TestGuiApp {
    fn launch(title: &str) -> Self {
        let script_path =
            std::env::temp_dir().join(format!("eyra_e2e_gui_{}.ps1", std::process::id()));
        let script = format!(
            r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$f = New-Object System.Windows.Forms.Form
$f.Text = "{title}"
$f.Size = New-Object System.Drawing.Size(600, 380)
$f.StartPosition = "Manual"
$f.Location = New-Object System.Drawing.Point(60, 60)

$lbl = New-Object System.Windows.Forms.Label
$lbl.Text = "Status: Ready"
$lbl.Location = New-Object System.Drawing.Point(20, 20)
$lbl.Size = New-Object System.Drawing.Size(520, 32)
$lbl.Font = New-Object System.Drawing.Font("Segoe UI", 13)
$f.Controls.Add($lbl)

$tb = New-Object System.Windows.Forms.TextBox
$tb.AccessibleName = "Query Input"
$tb.Location = New-Object System.Drawing.Point(20, 68)
$tb.Size = New-Object System.Drawing.Size(320, 30)
$f.Controls.Add($tb)

$pwd = New-Object System.Windows.Forms.TextBox
$pwd.AccessibleName = "Password Box"
$pwd.UseSystemPasswordChar = $true
$pwd.Text = "SuperSecret999"
$pwd.Location = New-Object System.Drawing.Point(20, 112)
$pwd.Size = New-Object System.Drawing.Size(320, 30)
$f.Controls.Add($pwd)

$chk = New-Object System.Windows.Forms.CheckBox
$chk.Text = "Enable Sync"
$chk.Location = New-Object System.Drawing.Point(20, 158)
$chk.Size = New-Object System.Drawing.Size(220, 30)
$f.Controls.Add($chk)

$btn = New-Object System.Windows.Forms.Button
$btn.Text = "Run Action"
$btn.Location = New-Object System.Drawing.Point(20, 204)
$btn.Size = New-Object System.Drawing.Size(160, 40)
$btn.Add_Click({{ $lbl.Text = "Updated:" + $tb.Text + ":" + $chk.Checked }})
$f.Controls.Add($btn)

$t = New-Object System.Windows.Forms.Timer
$t.Interval = 25000
$t.Add_Tick({{ $f.Close() }})
$t.Start()
[System.Windows.Forms.Application]::Run($f)
"#,
            title = title
        );
        std::fs::write(&script_path, script).expect("Write test WinForms script");

        let child = Command::new("powershell")
            .args([
                "-NoProfile",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-File",
                script_path.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Failed to spawn WinForms test app");

        Self {
            child,
            script_path,
            title: title.to_string(),
        }
    }
}

impl Drop for TestGuiApp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.script_path);
    }
}

fn rpc_roundtrip(
    stdin: &mut impl Write,
    reader: &mut BufReader<impl std::io::Read>,
    msg: &Value,
) -> Value {
    let line = serde_json::to_string(msg).unwrap();
    writeln!(stdin, "{}", line).unwrap();
    stdin.flush().unwrap();

    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    serde_json::from_str(resp_line.trim()).expect("Valid JSON-RPC response line")
}

fn find_id_by_label(see_text: &str, needle: &str) -> u32 {
    for line in see_text.lines() {
        if line.contains(needle) && line.starts_with('#') {
            if let Some(num_str) = line[1..].split_whitespace().next() {
                if let Ok(id) = num_str.parse::<u32>() {
                    return id;
                }
            }
        }
    }
    panic!(
        "Element containing {:?} not found in:\n{}",
        needle, see_text
    );
}

#[test]
fn mcp_full_eye_and_hand_pipeline_live() {
    let app = TestGuiApp::launch("EYRA MCP E2E Window");
    let log_path =
        std::env::temp_dir().join(format!("eyra_e2e_audit_{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&log_path);

    let mut mcp = Command::new(eyra_bin())
        .env("EYRA_CONFIRM", "off")
        .args(["--log-file", log_path.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn eyra mcp");

    let mut stdin = mcp.stdin.take().unwrap();
    let stdout = mcp.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // 1. Initialize + notification
    let init = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "eyra");
    writeln!(
        stdin,
        "{}",
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
    )
    .unwrap();
    stdin.flush().unwrap();

    // 2. Verify full toolset (16 tools: 5 eye + 5 ear/watch + 6 hand)
    let tools_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    assert_eq!(tools_resp["result"]["tools"].as_array().unwrap().len(), 16);

    // Wait for WinForms window to appear
    let mut found_window = false;
    for attempt in 0..35 {
        let win_resp = rpc_roundtrip(
            &mut stdin,
            &mut reader,
            &json!({
                "jsonrpc": "2.0",
                "id": 100 + attempt,
                "method": "tools/call",
                "params": { "name": "list_windows", "arguments": {} }
            }),
        );
        let text = win_resp["result"]["content"][0]["text"].as_str().unwrap();
        if text.contains(&app.title) {
            found_window = true;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    assert!(
        found_window,
        "Test GUI window did not appear in list_windows"
    );

    // 3. L1 UIA see + verify password secret is never leaked
    let see_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "see", "arguments": { "window": app.title } }
        }),
    );
    assert_eq!(see_resp["result"]["isError"], false);
    let see_text = see_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(see_text.contains("Status: Ready"));
    assert!(see_text.contains("pwd \"Password Box\""));
    assert!(!see_text.contains("SuperSecret999"));

    let input_id = find_id_by_label(see_text, "inp \"Query Input\"");
    let chk_id = find_id_by_label(see_text, "chk \"Enable Sync\"");
    let btn_id = find_id_by_label(see_text, "btn \"Run Action\"");

    // 4. L3 OBS-style screenshot with SoM
    let shot1 = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "screenshot",
                "arguments": { "window": app.title, "som": true, "max_width": 640, "quality": 65 }
            }
        }),
    );
    assert_eq!(shot1["result"]["isError"], false);
    let content1 = shot1["result"]["content"].as_array().unwrap();
    assert_eq!(content1.len(), 2);
    assert_eq!(content1[1]["type"], "image");
    assert_eq!(content1[1]["mimeType"], "image/jpeg");
    assert!(!content1[1]["data"].as_str().unwrap().is_empty());

    // 5. diff_only screenshot with no changes -> 0 image bytes (100% image token savings!)
    let shot_nodiff = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "screenshot",
                "arguments": { "window": app.title, "diff_only": true }
            }
        }),
    );
    assert_eq!(shot_nodiff["result"]["isError"], false);
    assert_eq!(
        shot_nodiff["result"]["content"].as_array().unwrap().len(),
        1
    );
    assert!(shot_nodiff["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("diff=0.0%"));

    // 6. Hand actions (background UIA typing, toggle, invoke)
    let type_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "type_text",
                "arguments": { "id": input_id, "text": "MCP_OK", "background": true }
            }
        }),
    );
    assert_eq!(type_resp["result"]["isError"], false);

    let chk_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "click",
                "arguments": { "id": chk_id, "background": true }
            }
        }),
    );
    assert_eq!(chk_resp["result"]["isError"], false);
    assert!(chk_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("TogglePattern"));

    let btn_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "click",
                "arguments": { "id": btn_id, "background": true }
            }
        }),
    );
    assert_eq!(btn_resp["result"]["isError"], false);

    thread::sleep(Duration::from_millis(120));

    // 7. see_diff returns compact changed-only lines confirming live mutation
    let diff_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": { "name": "see_diff", "arguments": { "window": app.title } }
        }),
    );
    assert_eq!(diff_resp["result"]["isError"], false);
    let diff_text = diff_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(diff_text.starts_with("DIFF "));
    assert!(
        diff_text.contains("Updated:MCP_OK:True"),
        "Expected updated label in diff:\n{}",
        diff_text
    );
    assert!(diff_text.contains("(checked)"));

    // 8. diff_only screenshot after UI change returns cropped diff image
    let shot_diff = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "screenshot",
                "arguments": { "window": app.title, "diff_only": true }
            }
        }),
    );
    assert_eq!(shot_diff["result"]["isError"], false);
    let diff_content = shot_diff["result"]["content"].as_array().unwrap();
    assert_eq!(diff_content.len(), 2);
    assert!(diff_content[0]["text"]
        .as_str()
        .unwrap()
        .contains("changed_rect="));

    // 9. Test Ear & Watch Together tools live (list_audio_devices + watch_start/poll/stop)
    let audio_devs = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": { "name": "list_audio_devices", "arguments": {} }
        }),
    );
    assert_eq!(audio_devs["result"]["isError"], false);
    assert!(audio_devs["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("AUDIO_DEVICES ("));

    let w_start = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "watch_start",
                "arguments": { "window": app.title, "audio": "system", "chunk_sec": 3.0 }
            }
        }),
    );
    assert_eq!(w_start["result"]["isError"], false);
    assert!(w_start["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("WATCH_START "));

    thread::sleep(Duration::from_millis(600));

    let w_poll = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": { "name": "watch_poll", "arguments": {} }
        }),
    );
    assert_eq!(w_poll["result"]["isError"], false);
    let poll_text = w_poll["result"]["content"][0]["text"].as_str().unwrap();
    assert!(poll_text.starts_with("WATCH session="));
    assert!(
        poll_text.contains("SCENE "),
        "Expected initial SCENE keyframe in watch_poll:\n{}",
        poll_text
    );

    let w_stop = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": { "name": "watch_stop", "arguments": { "keep_files": false } }
        }),
    );
    assert_eq!(w_stop["result"]["isError"], false);
    assert!(w_stop["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("cleaned up"));

    drop(stdin);
    assert!(mcp.wait().unwrap().success());

    // 10. Verify JSONL audit log recorded MCP actions with zero leaked secrets
    let log_content = std::fs::read_to_string(&log_path).expect("Read audit log");
    let entries: Vec<Value> = log_content
        .lines()
        .map(|l| serde_json::from_str(l).expect("Valid JSONL audit line"))
        .collect();
    assert!(entries.len() >= 8);
    assert!(entries
        .iter()
        .any(|e| e["action"] == "see" && e["source"] == "mcp"));
    assert!(entries
        .iter()
        .any(|e| e["action"] == "type_text" && e["ok"] == true));
    assert!(entries
        .iter()
        .any(|e| e["action"] == "click" && e["ok"] == true));
    assert!(entries
        .iter()
        .any(|e| e["action"] == "screenshot" && e["ok"] == true));
    assert!(!log_content.contains("SuperSecret999"));
    let _ = std::fs::remove_file(&log_path);
}

#[test]
fn mcp_read_only_mode_blocks_hand_tools() {
    let mut mcp = Command::new(eyra_bin())
        .env("EYRA_CONFIRM", "off")
        .args(["--no-log", "mcp", "--read-only"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to spawn eyra mcp --read-only");

    let mut stdin = mcp.stdin.take().unwrap();
    let stdout = mcp.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let tools_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
    );
    let tools = tools_resp["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 10);
    assert!(tools.iter().all(|t| t["name"] != "click"));

    let click_resp = rpc_roundtrip(
        &mut stdin,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "click", "arguments": { "x": 100, "y": 100 } }
        }),
    );
    assert_eq!(click_resp["result"]["isError"], true);
    assert!(click_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("read-only"));

    drop(stdin);
    assert!(mcp.wait().unwrap().success());
}
