use crate::types::WindowInfo;
use anyhow::{bail, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub read_only: bool,
    pub allow_sensitive: bool,
    pub corner_failsafe: bool,
    pub hotkey_failsafe: bool,
    /// Title phrases that mark a sensitive window, matched as whole words.
    pub sensitive_patterns: Vec<String>,
    /// Executable names (lowercase, exact match) of password managers and credential prompts.
    pub sensitive_processes: Vec<String>,
    /// When set, mouse/keyboard actions are allowed only in these apps (exe names, lowercase).
    pub allowed_apps: Option<Vec<String>>,
    /// Upper bound on input actions per minute (0 = unlimited). Also caps bursts at 10/second.
    pub max_actions_per_minute: u32,
    /// Which actions need the user's OK in a Windows dialog first.
    pub confirm: ConfirmMode,
    /// Microphone capture is off unless the user opts in (--allow-mic / EYRA_ALLOW_MIC).
    pub allow_mic: bool,
}

/// When EYRA asks the user before performing an input action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmMode {
    /// Never ask.
    Off,
    /// Ask before actions that look hard to undo: deleting, sending, paying, publishing...
    Risky,
    /// Ask before every input action.
    All,
}

impl ConfirmMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "never" | "0" | "false" => Some(Self::Off),
            "risky" | "default" => Some(Self::Risky),
            "all" | "always" => Some(Self::All),
            _ => None,
        }
    }
}

fn env_list(var: &str) -> Option<Vec<String>> {
    let v = std::env::var(var).ok()?;
    let list = parse_app_list(&v);
    (!list.is_empty()).then_some(list)
}

/// "chrome, Notepad.exe ,code" -> ["chrome.exe", "notepad.exe", "code.exe"]
pub fn parse_app_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|a| a.trim().to_lowercase())
        .filter(|a| !a.is_empty())
        .map(|a| {
            if a.ends_with(".exe") {
                a
            } else {
                format!("{}.exe", a)
            }
        })
        .collect()
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            read_only: std::env::var("EYRA_READ_ONLY")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            allow_sensitive: false,
            corner_failsafe: true,
            hotkey_failsafe: true,
            sensitive_patterns: default_sensitive_patterns(),
            sensitive_processes: default_sensitive_processes(),
            allowed_apps: env_list("EYRA_ALLOW_APPS"),
            max_actions_per_minute: std::env::var("EYRA_MAX_ACTIONS_PER_MIN")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(60),
            confirm: std::env::var("EYRA_CONFIRM")
                .ok()
                .and_then(|v| ConfirmMode::parse(&v))
                .unwrap_or(ConfirmMode::Risky),
            allow_mic: std::env::var("EYRA_ALLOW_MIC")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }
}

/// Title phrases (whole words) for password managers and Windows credential UI.
/// Deliberately specific: a plain word such as "keeper" would also match "Goalkeeper".
pub fn default_sensitive_patterns() -> Vec<String> {
    [
        "1password",
        "keepass",
        "keepassxc",
        "bitwarden",
        "lastpass",
        "dashlane",
        "enpass",
        "roboform",
        "nordpass",
        "proton pass",
        "keeper password manager",
        "keeper vault",
        "password manager",
        "credential manager",
        "windows security",
        "user account control",
        "authy",
        "authenticator",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Executables blocked regardless of their window title.
pub fn default_sensitive_processes() -> Vec<String> {
    [
        "1password.exe",
        "keepass.exe",
        "keepassxc.exe",
        "bitwarden.exe",
        "lastpass.exe",
        "dashlane.exe",
        "enpass.exe",
        "roboform.exe",
        "nordpass.exe",
        "keeperpasswordmanager.exe",
        "proton pass.exe",
        "protonpass.exe",
        "authy desktop.exe",
        "credentialuibroker.exe",
        "consent.exe",
        "sechealthui.exe",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

const BROWSER_PROCESSES: &[&str] = &[
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "arc.exe",
];

/// Browser pages that hold saved passwords. In a browser the window title is the active tab's
/// title, so a title segment equal to one of these marks the window sensitive.
const BROWSER_VAULT_PAGES: &[&str] = &[
    "passwords",
    "saved passwords",
    "password manager",
    "google password manager",
    "passkeys",
    "wallet",
    "microsoft wallet",
];

/// Words on a button or link that usually mean "hard to undo" (EN, AZ, TR, RU, DE, FR, ES).
const RISKY_WORDS: &[&str] = &[
    "delete",
    "remove",
    "erase",
    "discard",
    "destroy",
    "wipe",
    "empty trash",
    "uninstall",
    "format",
    "reset",
    "send",
    "submit",
    "publish",
    "post",
    "reply all",
    "pay",
    "purchase",
    "buy",
    "checkout",
    "place order",
    "order now",
    "transfer",
    "confirm",
    "sign out",
    "log out",
    "sil",
    "göndər",
    "ödə",
    "təsdiqlə",
    "yayımla",
    "satın al",
    "gönder",
    "öde",
    "onayla",
    "yayınla",
    "удалить",
    "отправить",
    "оплатить",
    "купить",
    "подтвердить",
    "опубликовать",
    "löschen",
    "senden",
    "kaufen",
    "bezahlen",
    "supprimer",
    "envoyer",
    "payer",
    "acheter",
    "eliminar",
    "enviar",
    "pagar",
    "comprar",
];

/// Hotkeys that delete permanently or send.
const RISKY_HOTKEYS: &[&str] = &["shift+delete", "ctrl+shift+delete", "ctrl+enter", "alt+s"];

/// Whether an element label (button, link, menu item) looks like a hard-to-undo action.
pub fn is_risky_label(label: &str) -> bool {
    let l = label.to_lowercase();
    RISKY_WORDS
        .iter()
        .any(|w| contains_whole_word_unicode(&l, w))
}

pub fn is_risky_hotkey(normalized: &[String]) -> bool {
    let combo = normalized.join("+").to_lowercase();
    RISKY_HOTKEYS.contains(&combo.as_str())
}

/// Unicode-aware whole-word match (for labels in any script).
fn contains_whole_word_unicode(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, m)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + m.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

/// `needle` occurs in `hay` with no letter or digit directly before or after it.
fn contains_whole_phrase(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let hay_b = hay.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = start == 0 || !hay_b[start - 1].is_ascii_alphanumeric();
        let after_ok = end >= hay_b.len() || !hay_b[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        while !hay.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

#[derive(Debug)]
pub struct SafetyGuard {
    pub config: SafetyConfig,
    hotkey_tripped: AtomicBool,
    recent_actions: Mutex<VecDeque<Instant>>,
}

impl SafetyGuard {
    pub fn new(config: SafetyConfig) -> Self {
        Self {
            config,
            hotkey_tripped: AtomicBool::new(false),
            recent_actions: Mutex::new(VecDeque::new()),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.config.read_only
    }

    pub fn is_sensitive_window(&self, title: &str, process: &str) -> bool {
        let title_low = title.to_lowercase();
        let proc_low = process
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(process)
            .to_lowercase();

        // 1. Known password-manager / credential executables, exact name
        if self
            .config
            .sensitive_processes
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&proc_low))
        {
            return true;
        }
        // 2. Title phrases as whole words ("Bitwarden" yes, "Goalkeeper" no)
        if self
            .config
            .sensitive_patterns
            .iter()
            .any(|p| contains_whole_phrase(&title_low, &p.to_lowercase()))
        {
            return true;
        }
        // 3. Password pages inside a browser ("Google Password Manager", "Passwords", "Wallet")
        if BROWSER_PROCESSES.contains(&proc_low.as_str()) {
            return title_low
                .split(" - ")
                .flat_map(|seg| seg.split(" | "))
                .flat_map(|seg| seg.split(" \u{2014} "))
                .map(str::trim)
                .any(|seg| BROWSER_VAULT_PAGES.contains(&seg));
        }
        false
    }

    pub fn should_redact_window(&self, win: &WindowInfo) -> bool {
        win.is_sensitive && !self.config.allow_sensitive
    }

    pub fn check_failsafe(&self) -> Result<()> {
        if self.hotkey_tripped.load(Ordering::SeqCst) {
            bail!("Safety failsafe is tripped (Ctrl+Alt+Esc was pressed). Restart eyra to resume.");
        }

        if self.config.hotkey_failsafe && is_emergency_hotkey_pressed() {
            self.hotkey_tripped.store(true, Ordering::SeqCst);
            bail!("Emergency hotkey failsafe triggered (Ctrl+Alt+Esc). All input actions halted.");
        }

        if self.config.corner_failsafe {
            if let Some((x, y)) = get_cursor_pos() {
                if x <= 2 && y <= 2 {
                    bail!(
                        "Emergency corner failsafe triggered (cursor at {},{}). Move cursor away from top-left corner to resume.",
                        x,
                        y
                    );
                }
            }
        }

        Ok(())
    }

    pub fn check_write_allowed(&self, target_window: Option<&WindowInfo>) -> Result<()> {
        if self.config.read_only {
            bail!("Action blocked: eyra is running in --read-only mode.");
        }

        self.check_failsafe()?;

        if let Some(win) = target_window {
            if win.is_sensitive && !self.config.allow_sensitive {
                bail!(
                    "Action blocked: target window \"{}\" ({}) is protected as sensitive. Start with --allow-sensitive to override.",
                    win.title,
                    win.process
                );
            }
        }

        self.check_app_allowed(target_window)?;
        self.check_rate_limit()?;
        Ok(())
    }

    /// `--allow-apps`: input actions only in the listed executables.
    pub fn check_app_allowed(&self, target_window: Option<&WindowInfo>) -> Result<()> {
        let Some(allowed) = &self.config.allowed_apps else {
            return Ok(());
        };
        let Some(win) = target_window else {
            bail!(
                "Action blocked: --allow-apps is set ({}), but the target window couldn't be determined. Pass window=... or click by #id.",
                allowed.join(", ")
            );
        };
        let process = win.process.to_lowercase();
        if allowed.contains(&process) {
            Ok(())
        } else {
            bail!(
                "Action blocked: {} is not in the allowed apps ({}). Restart eyra with --allow-apps to change the list.",
                win.process,
                allowed.join(", ")
            )
        }
    }

    /// Stops a runaway agent: at most `max_actions_per_minute` actions per 60 s and 10 per second.
    pub fn check_rate_limit(&self) -> Result<()> {
        let per_minute = self.config.max_actions_per_minute;
        if per_minute == 0 {
            return Ok(());
        }
        let now = Instant::now();
        let mut q = self
            .recent_actions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            q.pop_front();
        }
        let last_second = q
            .iter()
            .filter(|t| now.duration_since(**t) < Duration::from_secs(1))
            .count();
        if q.len() >= per_minute as usize {
            bail!(
                "Action blocked: rate limit reached ({} actions per minute). Wait a moment, or raise it with --max-actions-per-minute.",
                per_minute
            );
        }
        if last_second >= 10 {
            bail!("Action blocked: more than 10 actions in one second. Slow down.");
        }
        q.push_back(now);
        Ok(())
    }

    /// Shows a Windows Yes/No dialog when the confirm mode requires it; "No" blocks the action.
    pub fn confirm_action(&self, description: &str, risky: bool) -> Result<()> {
        let needed = match self.config.confirm {
            ConfirmMode::Off => false,
            ConfirmMode::Risky => risky,
            ConfirmMode::All => true,
        };
        if !needed {
            return Ok(());
        }
        use windows::core::HSTRING;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST,
            MB_YESNO,
        };
        let text = HSTRING::from(format!(
            "An AI agent wants to:\n\n{}\n\nAllow this action?",
            description
        ));
        let caption = HSTRING::from("EYRA: confirm action");
        let answer = unsafe {
            MessageBoxW(
                None,
                &text,
                &caption,
                MB_YESNO | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND | MB_DEFBUTTON2,
            )
        };
        if answer == IDYES {
            Ok(())
        } else {
            bail!("Action declined by the user: {}", description)
        }
    }

    pub fn check_hotkey_allowed(
        &self,
        normalized_keys: &[String],
        target_window: Option<&WindowInfo>,
    ) -> Result<()> {
        self.check_write_allowed(target_window)?;

        let has = |k: &str| normalized_keys.iter().any(|x| x == k);
        if has("ctrl") && has("alt") && has("delete") {
            bail!("Blocked dangerous system hotkey: ctrl+alt+delete.");
        }
        if has("win") && has("l") {
            bail!("Blocked system lock hotkey: win+l.");
        }
        if has("alt") && has("f4") {
            if let Some(w) = target_window {
                let p = w.process.to_ascii_lowercase();
                if p == "explorer.exe" {
                    bail!("Blocked alt+f4 on explorer.exe (shell shutdown dialog).");
                }
            }
        }
        Ok(())
    }
}

fn get_cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    unsafe {
        if GetCursorPos(&mut pt).is_ok() {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

fn is_emergency_hotkey_pressed() -> bool {
    unsafe {
        let ctrl = (GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        let alt = (GetAsyncKeyState(VK_MENU.0 as i32) as u16 & 0x8000) != 0;
        let esc = (GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000) != 0;
        ctrl && alt && esc
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Rect;

    #[test]
    fn sensitive_window_detection_works() {
        let guard = SafetyGuard::new(SafetyConfig::default());
        assert!(guard.is_sensitive_window("Vault - Bitwarden", "chrome.exe"));
        assert!(guard.is_sensitive_window("My DB", "KeePassXC.exe"));
        assert!(guard.is_sensitive_window("Windows Security", "CredentialUIBroker.exe"));
        assert!(!guard.is_sensitive_window("GitHub - Google Chrome", "chrome.exe"));
    }

    fn win(process: &str) -> WindowInfo {
        WindowInfo {
            hwnd: 1,
            title: "t".into(),
            process: process.into(),
            pid: 1,
            rect: Rect::new(0, 0, 10, 10),
            is_foreground: true,
            is_minimized: false,
            is_sensitive: false,
        }
    }

    fn guard_with(f: impl FnOnce(&mut SafetyConfig)) -> SafetyGuard {
        let mut cfg = SafetyConfig {
            read_only: false,
            corner_failsafe: false,
            hotkey_failsafe: false,
            allowed_apps: None,
            confirm: ConfirmMode::Off,
            ..Default::default()
        };
        f(&mut cfg);
        SafetyGuard::new(cfg)
    }

    #[test]
    fn allow_apps_limits_actions_to_listed_processes() {
        assert_eq!(
            parse_app_list(" chrome, Notepad.exe ,,code"),
            vec!["chrome.exe", "notepad.exe", "code.exe"]
        );
        let g = guard_with(|c| c.allowed_apps = Some(parse_app_list("chrome,notepad")));
        assert!(g.check_write_allowed(Some(&win("Notepad.exe"))).is_ok());
        assert!(g.check_write_allowed(Some(&win("explorer.exe"))).is_err());
        // unknown target is refused rather than allowed
        assert!(g.check_write_allowed(None).is_err());
        // without a list, everything (non-sensitive) is allowed
        let open = guard_with(|_| {});
        assert!(open.check_write_allowed(Some(&win("explorer.exe"))).is_ok());
    }

    #[test]
    fn rate_limit_caps_bursts_and_minutes() {
        let g = guard_with(|c| c.max_actions_per_minute = 5);
        for _ in 0..5 {
            assert!(g.check_rate_limit().is_ok());
        }
        assert!(
            g.check_rate_limit().is_err(),
            "6th action within a minute must be blocked"
        );

        let burst = guard_with(|c| c.max_actions_per_minute = 1000);
        for _ in 0..10 {
            assert!(burst.check_rate_limit().is_ok());
        }
        assert!(
            burst.check_rate_limit().is_err(),
            "11th action within a second must be blocked"
        );

        let unlimited = guard_with(|c| c.max_actions_per_minute = 0);
        for _ in 0..50 {
            assert!(unlimited.check_rate_limit().is_ok());
        }
    }

    #[test]
    fn risky_labels_and_hotkeys() {
        for l in [
            "Delete",
            "Send",
            "Pay now",
            "Place order",
            "Sil",
            "Göndər",
            "Удалить",
            "Empty trash",
        ] {
            assert!(is_risky_label(l), "{} should be risky", l);
        }
        for l in [
            "Search",
            "Settings",
            "Sender name",
            "Postcode",
            "Deleted items",
            "Silver",
            "Run Action",
        ] {
            assert!(!is_risky_label(l), "{} should not be risky", l);
        }
        assert!(is_risky_hotkey(&["shift".into(), "delete".into()]));
        assert!(!is_risky_hotkey(&["ctrl".into(), "c".into()]));
        assert_eq!(ConfirmMode::parse("ALL"), Some(ConfirmMode::All));
        assert_eq!(ConfirmMode::parse("off"), Some(ConfirmMode::Off));
        // confirm mode Off never shows a dialog
        assert!(guard_with(|_| {})
            .confirm_action("click Delete", true)
            .is_ok());
    }

    #[test]
    fn process_names_match_exactly_even_with_a_neutral_title() {
        let guard = SafetyGuard::new(SafetyConfig::default());
        assert!(guard.is_sensitive_window("Untitled", "1Password.exe"));
        assert!(guard.is_sensitive_window("", r"C:\\Program Files\\KeePass\\KeePass.exe"));
        // a process merely containing a manager's name is not blocked
        assert!(!guard.is_sensitive_window("Notes", "keepassnotes-helper.exe"));
    }

    #[test]
    fn title_phrases_need_whole_words() {
        let guard = SafetyGuard::new(SafetyConfig::default());
        // previously blocked by the plain substring "keeper"
        assert!(!guard.is_sensitive_window(
            "Goalkeeper highlights - YouTube - Google Chrome",
            "chrome.exe"
        ));
        assert!(!guard.is_sensitive_window("Apache ZooKeeper docs", "notepad.exe"));
        assert!(!guard.is_sensitive_window("MyAuthenticatorApp - Visual Studio", "devenv.exe"));
        assert!(guard.is_sensitive_window("Keeper Password Manager", "chrome.exe"));
        assert!(guard.is_sensitive_window("Microsoft Authenticator", "app.exe"));
    }

    #[test]
    fn browser_password_pages_are_sensitive() {
        let guard = SafetyGuard::new(SafetyConfig::default());
        assert!(guard.is_sensitive_window("Google Password Manager - Google Chrome", "chrome.exe"));
        assert!(guard.is_sensitive_window("Settings - Passwords - Microsoft Edge", "msedge.exe"));
        assert!(guard.is_sensitive_window("Wallet - Profile 1 - Microsoft Edge", "msedge.exe"));
        assert!(guard.is_sensitive_window(
            "Vaults | Bitwarden Web vault - Mozilla Firefox",
            "firefox.exe"
        ));
        // the same words outside a browser, or as part of a longer page title, are fine
        assert!(!guard.is_sensitive_window("Passwords - Notepad", "notepad.exe"));
        assert!(!guard.is_sensitive_window(
            "How to choose strong passwords - Wikipedia - Google Chrome",
            "chrome.exe"
        ));
    }

    #[test]
    fn read_only_blocks_write() {
        let cfg = SafetyConfig {
            read_only: true,
            corner_failsafe: false,
            hotkey_failsafe: false,
            ..Default::default()
        };
        let guard = SafetyGuard::new(cfg);
        assert!(guard.check_write_allowed(None).is_err());
    }

    #[test]
    fn sensitive_window_blocks_write_unless_allowed() {
        let cfg = SafetyConfig {
            read_only: false,
            corner_failsafe: false,
            hotkey_failsafe: false,
            ..Default::default()
        };
        let guard = SafetyGuard::new(cfg);

        let win = WindowInfo {
            hwnd: 1,
            title: "1Password".into(),
            process: "1password.exe".into(),
            pid: 100,
            rect: Rect::new(0, 0, 500, 400),
            is_foreground: true,
            is_minimized: false,
            is_sensitive: true,
        };
        assert!(guard.check_write_allowed(Some(&win)).is_err());
    }

    #[test]
    fn dangerous_hotkeys_are_blocked() {
        let cfg = SafetyConfig {
            read_only: false,
            corner_failsafe: false,
            hotkey_failsafe: false,
            ..Default::default()
        };
        let guard = SafetyGuard::new(cfg);

        let cad = vec!["ctrl".into(), "alt".into(), "delete".into()];
        assert!(guard.check_hotkey_allowed(&cad, None).is_err());

        let win_l = vec!["win".into(), "l".into()];
        assert!(guard.check_hotkey_allowed(&win_l, None).is_err());

        let copy = vec!["ctrl".into(), "c".into()];
        assert!(guard.check_hotkey_allowed(&copy, None).is_ok());
    }
}
