//! `eyra setup`: add EYRA to the MCP configuration of AI apps (Antigravity, Gemini CLI, Cursor,
//! VS Code, Windsurf, Claude Desktop) so users don't have to edit JSON by hand.
//!
//! Safety rules: other servers and settings in the file are kept as they are (key order too),
//! a timestamped backup is written before any change, files that aren't plain JSON (for example
//! JSON with comments) are never rewritten, and the write is atomic (temp file + rename).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Name of EYRA's entry inside each app's server map.
pub const SERVER_NAME: &str = "eyra";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Antigravity,
    GeminiCli,
    Cursor,
    VsCode,
    Windsurf,
    ClaudeDesktop,
    ClaudeCode,
}

pub const ALL_HOSTS: [Host; 7] = [
    Host::Antigravity,
    Host::GeminiCli,
    Host::Cursor,
    Host::VsCode,
    Host::Windsurf,
    Host::ClaudeDesktop,
    Host::ClaudeCode,
];

impl Host {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "antigravity" | "google-antigravity" => Some(Self::Antigravity),
            "gemini" | "gemini-cli" => Some(Self::GeminiCli),
            "cursor" => Some(Self::Cursor),
            "vscode" | "vs-code" | "code" => Some(Self::VsCode),
            "windsurf" => Some(Self::Windsurf),
            "claude" | "claude-desktop" => Some(Self::ClaudeDesktop),
            "claude-code" => Some(Self::ClaudeCode),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Antigravity => "antigravity",
            Self::GeminiCli => "gemini-cli",
            Self::Cursor => "cursor",
            Self::VsCode => "vscode",
            Self::Windsurf => "windsurf",
            Self::ClaudeDesktop => "claude-desktop",
            Self::ClaudeCode => "claude-code",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Antigravity => "Google Antigravity",
            Self::GeminiCli => "Gemini CLI",
            Self::Cursor => "Cursor",
            Self::VsCode => "VS Code",
            Self::Windsurf => "Windsurf",
            Self::ClaudeDesktop => "Claude Desktop",
            Self::ClaudeCode => "Claude Code",
        }
    }

    /// Top-level key that holds the server map. VS Code uses `servers`, everyone else `mcpServers`.
    pub fn servers_key(self) -> &'static str {
        match self {
            Self::VsCode => "servers",
            _ => "mcpServers",
        }
    }

    /// Windsurf's Windows path isn't documented as clearly as the others, so EYRA only edits
    /// a Windsurf config that already exists instead of creating one in a guessed location.
    fn requires_existing_file(self) -> bool {
        matches!(self, Self::Windsurf)
    }

    /// The JSON file this app reads its MCP servers from (`None` for Claude Code, which is
    /// configured through its own CLI or plugin).
    pub fn config_path(self) -> Option<PathBuf> {
        let home = env_dir("USERPROFILE")?;
        match self {
            // Current Antigravity builds read ~/.gemini/config/mcp_config.json; earlier builds
            // used ~/.gemini/antigravity/mcp_config.json. Prefer whichever already exists.
            Self::Antigravity => {
                let current = home.join(".gemini").join("config").join("mcp_config.json");
                let legacy = home
                    .join(".gemini")
                    .join("antigravity")
                    .join("mcp_config.json");
                Some(if !current.exists() && legacy.exists() {
                    legacy
                } else {
                    current
                })
            }
            Self::GeminiCli => Some(home.join(".gemini").join("settings.json")),
            Self::Cursor => Some(home.join(".cursor").join("mcp.json")),
            Self::VsCode => Some(
                env_dir("APPDATA")?
                    .join("Code")
                    .join("User")
                    .join("mcp.json"),
            ),
            Self::Windsurf => Some(
                home.join(".codeium")
                    .join("windsurf")
                    .join("mcp_config.json"),
            ),
            // The Microsoft Store (MSIX) build reads a virtualized copy of %APPDATA%\Claude.
            Self::ClaudeDesktop => {
                let file = Path::new("Claude").join("claude_desktop_config.json");
                if let Some(pkg) = claude_msix_package_dir() {
                    return Some(pkg.join("LocalCache").join("Roaming").join(file));
                }
                Some(env_dir("APPDATA")?.join(file))
            }
            Self::ClaudeCode => None,
        }
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn claude_msix_package_dir() -> Option<PathBuf> {
    let packages = env_dir("LOCALAPPDATA")?.join("Packages");
    fs::read_dir(packages).ok()?.flatten().find_map(|e| {
        let name = e.file_name().to_string_lossy().to_string();
        (name.starts_with("Claude_") && e.path().is_dir()).then(|| e.path())
    })
}

/// The server entry EYRA writes for a host.
pub fn server_entry(host: Host, exe: &Path, read_only: bool) -> Value {
    let mut args = vec!["mcp".to_string()];
    if read_only {
        args.push("--read-only".to_string());
    }
    let command = exe.to_string_lossy().to_string();
    match host {
        Host::VsCode => json!({ "type": "stdio", "command": command, "args": args }),
        _ => json!({ "command": command, "args": args }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Unchanged,
    Removed,
    NotPresent,
}

/// Pure core of `setup`: takes the current file text (or `None` if it doesn't exist) and returns
/// the new text plus what changed. `entry = None` removes EYRA's entry.
pub fn apply_to_document(
    current: Option<&str>,
    servers_key: &str,
    entry: Option<&Value>,
) -> Result<(String, Change)> {
    let mut doc: Value = match current.map(str::trim).filter(|s| !s.is_empty()) {
        None => Value::Object(Map::new()),
        Some(text) => serde_json::from_str(text).map_err(|e| {
            anyhow!(
                "the file isn't plain JSON ({}). It may contain comments; EYRA won't rewrite it",
                e
            )
        })?,
    };
    let root = doc
        .as_object_mut()
        .ok_or_else(|| anyhow!("the file's top level isn't a JSON object"))?;

    if !root.contains_key(servers_key) {
        if entry.is_none() {
            return Ok((current.unwrap_or_default().to_string(), Change::NotPresent));
        }
        root.insert(servers_key.to_string(), Value::Object(Map::new()));
    }
    let servers = root
        .get_mut(servers_key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("\"{}\" isn't a JSON object", servers_key))?;

    // When updating, keep any extra fields the user added to EYRA's entry (env, timeout, ...)
    // and only replace the fields EYRA owns.
    let merged = match (entry, servers.get(SERVER_NAME)) {
        (Some(Value::Object(new)), Some(Value::Object(old))) => {
            let mut m = old.clone();
            for (k, v) in new {
                m.insert(k.clone(), v.clone());
            }
            Some(Value::Object(m))
        }
        (Some(new), _) => Some(new.clone()),
        (None, _) => None,
    };

    let change = match (merged, servers.get(SERVER_NAME)) {
        (Some(new), Some(old)) if *old == new => Change::Unchanged,
        (Some(new), Some(_)) => {
            servers.insert(SERVER_NAME.to_string(), new);
            Change::Updated
        }
        (Some(new), None) => {
            servers.insert(SERVER_NAME.to_string(), new);
            Change::Added
        }
        (None, Some(_)) => {
            servers.remove(SERVER_NAME);
            Change::Removed
        }
        (None, None) => Change::NotPresent,
    };

    if matches!(change, Change::Unchanged | Change::NotPresent) {
        return Ok((current.unwrap_or_default().to_string(), change));
    }
    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    Ok((text, change))
}

/// Whether the file at `path` already has an `eyra` entry.
fn has_eyra(path: &Path, servers_key: &str) -> Option<bool> {
    let text = fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    Some(doc.get(servers_key)?.get(SERVER_NAME).is_some())
}

pub struct SetupOptions {
    pub read_only: bool,
    pub remove: bool,
    pub dry_run: bool,
    pub exe: Option<PathBuf>,
}

pub fn run_setup(host: Host, opts: &SetupOptions) -> Result<String> {
    let exe = match &opts.exe {
        Some(p) => p.clone(),
        None => std::env::current_exe().context("can't find eyra.exe's own path")?,
    };
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let exe = PathBuf::from(exe.to_string_lossy().trim_start_matches(r"\\?\"));

    if host == Host::ClaudeCode {
        return Ok(format!(
            "Claude Code is configured through its own CLI. Run one of:\n  /plugin install eyra --marketplace layer8labs8/EYRA\n  claude mcp add eyra -- \"{}\" mcp{}",
            exe.display(),
            if opts.read_only { " --read-only" } else { "" }
        ));
    }

    let path = host
        .config_path()
        .ok_or_else(|| anyhow!("can't work out the config folder (USERPROFILE/APPDATA not set)"))?;
    let exists = path.exists();
    if !exists && host.requires_existing_file() && !opts.remove {
        bail!(
            "{} config not found at {}. Open {} once (or add an MCP server in its settings) and run this again.",
            host.display_name(),
            path.display(),
            host.display_name()
        );
    }

    let current = if exists {
        Some(fs::read_to_string(&path).with_context(|| format!("can't read {}", path.display()))?)
    } else {
        None
    };
    let entry = (!opts.remove).then(|| server_entry(host, &exe, opts.read_only));
    let (new_text, change) =
        apply_to_document(current.as_deref(), host.servers_key(), entry.as_ref()).map_err(|e| {
            anyhow!(
                "{}: {}.\nAdd this under \"{}\" by hand instead:\n  \"{}\": {}",
                path.display(),
                e,
                host.servers_key(),
                SERVER_NAME,
                server_entry(host, &exe, opts.read_only)
            )
        })?;

    let mut out = String::new();
    let verb = match change {
        Change::Added => "added to",
        Change::Updated => "updated in",
        Change::Unchanged => "already up to date in",
        Change::Removed => "removed from",
        Change::NotPresent => "was not configured in",
    };

    // The entry as it will end up in the file (extra fields the user added are kept)
    let final_entry = serde_json::from_str::<Value>(&new_text)
        .ok()
        .and_then(|d| d.get(host.servers_key())?.get(SERVER_NAME).cloned());

    if opts.dry_run {
        let what = match change {
            Change::Added => "would be added to",
            Change::Updated => "would be updated in",
            Change::Unchanged => "is already up to date in",
            Change::Removed => "would be removed from",
            Change::NotPresent => "is not configured in",
        };
        let _ = writeln!(
            out,
            "DRY RUN (nothing written): EYRA {} {}",
            what,
            path.display()
        );
        if let Some(e) = &final_entry {
            let _ = writeln!(out, "  \"{}\": {}", SERVER_NAME, e);
        }
        return Ok(out);
    }

    if matches!(change, Change::Unchanged | Change::NotPresent) {
        let _ = writeln!(out, "EYRA {} {}", verb, path.display());
        return Ok(out);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("can't create {}", parent.display()))?;
    }
    let mut backup_note = String::new();
    if exists {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let base = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config")
            .to_string();
        // Never overwrite an earlier backup, even when two runs land in the same millisecond
        let mut backup = path.with_file_name(format!("{}.eyra-backup-{}", base, stamp));
        let mut n = 1;
        while backup.exists() {
            backup = path.with_file_name(format!("{}.eyra-backup-{}-{}", base, stamp, n));
            n += 1;
        }
        fs::copy(&path, &backup).with_context(|| format!("can't back up {}", path.display()))?;
        backup_note = format!(" (backup: {})", backup.display());
    }
    let tmp = path.with_extension("eyra-tmp");
    fs::write(&tmp, &new_text).with_context(|| format!("can't write {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("can't replace {}", path.display()))?;

    let _ = writeln!(out, "EYRA {} {}{}", verb, path.display(), backup_note);
    if let Some(e) = &final_entry {
        let _ = writeln!(out, "  \"{}\": {}", SERVER_NAME, e);
    }
    let _ = writeln!(
        out,
        "Restart {} (or reload its MCP servers) to pick up the change.",
        host.display_name()
    );
    Ok(out)
}

/// `eyra setup --list`: every supported app, where its config lives and whether EYRA is in it.
pub fn list_text() -> String {
    let mut out = String::from("SUPPORTED APPS (eyra setup <app>):\n");
    for host in ALL_HOSTS {
        let status = match host.config_path() {
            None => "configure with its CLI or plugin".to_string(),
            Some(p) => {
                let state = if !p.exists() {
                    "no config file yet"
                } else {
                    match has_eyra(&p, host.servers_key()) {
                        Some(true) => "EYRA configured",
                        Some(false) => "config found, EYRA not added",
                        None => "config found (not plain JSON)",
                    }
                };
                format!("{} | {}", state, p.display())
            }
        };
        let _ = writeln!(
            out,
            "  {:<15} {:<19} {}",
            host.id(),
            host.display_name(),
            status
        );
    }
    out.push_str(
        "Options: --read-only (no mouse/keyboard tools), --remove, --dry-run, --exe <path>\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Tools\eyra\eyra.exe")
    }

    #[test]
    fn adds_entry_to_missing_file() {
        let e = server_entry(Host::Cursor, &exe(), false);
        let (text, change) = apply_to_document(None, "mcpServers", Some(&e)).unwrap();
        assert_eq!(change, Change::Added);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["mcpServers"]["eyra"]["command"],
            r"C:\Tools\eyra\eyra.exe"
        );
        assert_eq!(v["mcpServers"]["eyra"]["args"], json!(["mcp"]));
    }

    #[test]
    fn keeps_other_servers_settings_and_key_order() {
        let before = r#"{"theme":"dark","mcpServers":{"zeta":{"command":"z.exe"},"alpha":{"command":"a.exe"}}}"#;
        let e = server_entry(Host::Antigravity, &exe(), true);
        let (text, change) = apply_to_document(Some(before), "mcpServers", Some(&e)).unwrap();
        assert_eq!(change, Change::Added);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["zeta"]["command"], "z.exe");
        assert_eq!(
            v["mcpServers"]["eyra"]["args"],
            json!(["mcp", "--read-only"])
        );
        // existing order preserved, eyra appended last
        let names: Vec<&String> = v["mcpServers"].as_object().unwrap().keys().collect();
        assert_eq!(names, vec!["zeta", "alpha", "eyra"]);
        assert!(text.find("\"theme\"").unwrap() < text.find("\"mcpServers\"").unwrap());
    }

    #[test]
    fn update_keeps_user_added_fields_like_env() {
        let before = r#"{"mcpServers":{"eyra":{"command":"C:\\old\\eyra.exe","args":["mcp"],"env":{"EYRA_LOG_FILE":"x.jsonl"}}}}"#;
        let e = server_entry(Host::Antigravity, &exe(), false);
        let (text, change) = apply_to_document(Some(before), "mcpServers", Some(&e)).unwrap();
        assert_eq!(change, Change::Updated);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["mcpServers"]["eyra"]["command"],
            r"C:\Tools\eyra\eyra.exe"
        );
        assert_eq!(v["mcpServers"]["eyra"]["env"]["EYRA_LOG_FILE"], "x.jsonl");
        // running again with the same exe is a no-op even though the file has the extra field
        let (_, again) = apply_to_document(Some(&text), "mcpServers", Some(&e)).unwrap();
        assert_eq!(again, Change::Unchanged);
    }

    #[test]
    fn vscode_uses_servers_key_and_stdio_type() {
        let e = server_entry(Host::VsCode, &exe(), false);
        let (text, _) = apply_to_document(None, Host::VsCode.servers_key(), Some(&e)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["servers"]["eyra"]["type"], "stdio");
    }

    #[test]
    fn rerun_is_unchanged_and_update_and_remove_work() {
        let e = server_entry(Host::Cursor, &exe(), false);
        let (first, _) = apply_to_document(None, "mcpServers", Some(&e)).unwrap();
        let (_, again) = apply_to_document(Some(&first), "mcpServers", Some(&e)).unwrap();
        assert_eq!(again, Change::Unchanged);

        let ro = server_entry(Host::Cursor, &exe(), true);
        let (second, upd) = apply_to_document(Some(&first), "mcpServers", Some(&ro)).unwrap();
        assert_eq!(upd, Change::Updated);

        let (third, rem) = apply_to_document(Some(&second), "mcpServers", None).unwrap();
        assert_eq!(rem, Change::Removed);
        let v: Value = serde_json::from_str(&third).unwrap();
        assert!(v["mcpServers"].get("eyra").is_none());
    }

    #[test]
    fn refuses_json_with_comments_and_non_objects() {
        let jsonc = "{\n  // my servers\n  \"servers\": {}\n}";
        let e = server_entry(Host::VsCode, &exe(), false);
        assert!(apply_to_document(Some(jsonc), "servers", Some(&e)).is_err());
        assert!(apply_to_document(Some("[1,2]"), "servers", Some(&e)).is_err());
        assert!(apply_to_document(Some(r#"{"servers": 5}"#), "servers", Some(&e)).is_err());
    }

    #[test]
    fn host_names_parse() {
        assert_eq!(Host::parse("Antigravity"), Some(Host::Antigravity));
        assert_eq!(Host::parse("gemini"), Some(Host::GeminiCli));
        assert_eq!(Host::parse("VS_Code"), Some(Host::VsCode));
        assert_eq!(Host::parse("claude"), Some(Host::ClaudeDesktop));
        assert_eq!(Host::parse("nope"), None);
    }
}
