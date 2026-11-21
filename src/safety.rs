use crate::types::WindowInfo;
use anyhow::{bail, Result};
use std::sync::atomic::{AtomicBool, Ordering};
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
    pub sensitive_patterns: Vec<String>,
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
        }
    }
}

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
        "proton pass",
        "authy",
        "authenticator",
        "credential manager",
        "windows security",
        "user account control",
        "password manager",
        "nordpass",
        "keeper",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Debug)]
pub struct SafetyGuard {
    pub config: SafetyConfig,
    hotkey_tripped: AtomicBool,
}

impl SafetyGuard {
    pub fn new(config: SafetyConfig) -> Self {
        Self {
            config,
            hotkey_tripped: AtomicBool::new(false),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.config.read_only
    }

    pub fn is_sensitive_window(&self, title: &str, process: &str) -> bool {
        let title_low = title.to_ascii_lowercase();
        let proc_low = process.to_ascii_lowercase();
        self.config.sensitive_patterns.iter().any(|pat| {
            let p = pat.to_ascii_lowercase();
            title_low.contains(&p) || proc_low.contains(&p)
        })
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

        Ok(())
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
