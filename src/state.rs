use crate::audit::AuditLogger;
use crate::safety::{SafetyConfig, SafetyGuard};
use crate::types::{CapturedFrame, SeeSnapshot, UiElement};
use crate::watch::WatchSession;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct AppState {
    pub safety: SafetyGuard,
    pub audit: AuditLogger,
    pub last_snapshot: Option<SeeSnapshot>,
    /// Most recent snapshot per target (`Some(hwnd)` or `None` for the whole screen);
    /// used as the `see_diff` baseline.
    pub snapshots: HashMap<Option<usize>, SeeSnapshot>,
    pub last_frame: Option<CapturedFrame>,
    pub locked_window: Option<usize>,
    pub watch_session: Option<WatchSession>,
}

impl AppState {
    #[allow(dead_code)]
    pub fn new(safety_config: SafetyConfig) -> Self {
        Self::new_with_audit(safety_config, AuditLogger::disabled())
    }

    pub fn new_with_audit(safety_config: SafetyConfig, audit: AuditLogger) -> Self {
        Self {
            safety: SafetyGuard::new(safety_config),
            audit,
            last_snapshot: None,
            snapshots: HashMap::new(),
            last_frame: None,
            locked_window: None,
            watch_session: None,
        }
    }

    /// Records a snapshot as both the active one (for `click`/`type` by `#id`) and the
    /// per-window diff baseline. Keeps at most 16 window baselines.
    pub fn remember_snapshot(&mut self, snap: SeeSnapshot) {
        let key = snap.window.as_ref().map(|w| w.hwnd);
        if self.snapshots.len() >= 16 && !self.snapshots.contains_key(&key) {
            if let Some(k) = self.snapshots.keys().next().copied() {
                self.snapshots.remove(&k);
            }
        }
        self.snapshots.insert(key, snap.clone());
        self.last_snapshot = Some(snap);
    }

    pub fn find_element(&self, id: u32) -> Option<(&UiElement, Option<usize>)> {
        let snap = self.last_snapshot.as_ref()?;
        let el = snap.elements.iter().find(|e| e.id == id)?;
        let hwnd = snap.window.as_ref().map(|w| w.hwnd);
        Some((el, hwnd))
    }

    pub fn is_password_element(&self, id: Option<u32>) -> bool {
        let Some(eid) = id else {
            return false;
        };
        self.find_element(eid)
            .map(|(el, _)| el.is_password || el.role == "pwd")
            .unwrap_or(false)
    }

    pub fn default_cli_state_path() -> PathBuf {
        std::env::temp_dir().join("eyra_cli_state.json")
    }

    /// CLI state file: the active snapshot first, followed by the other per-window baselines.
    pub fn load_snapshot_from_file(&mut self, path: &Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let snaps: Vec<SeeSnapshot> = match serde_json::from_slice::<Vec<SeeSnapshot>>(&bytes) {
            Ok(v) => v,
            // Older eyra versions stored a single snapshot object
            Err(_) => match serde_json::from_slice::<SeeSnapshot>(&bytes) {
                Ok(s) => vec![s],
                Err(_) => return,
            },
        };
        for snap in snaps.into_iter().rev() {
            self.remember_snapshot(snap);
        }
    }

    pub fn save_snapshot_to_file(&self, path: &Path) {
        let Some(active) = &self.last_snapshot else {
            return;
        };
        let active_key = active.window.as_ref().map(|w| w.hwnd);
        let mut all = vec![active];
        all.extend(
            self.snapshots
                .iter()
                .filter(|(k, _)| **k != active_key)
                .map(|(_, v)| v),
        );
        if let Ok(json) = serde_json::to_vec(&all) {
            let _ = std::fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::SafetyConfig;
    use crate::types::{Rect, WindowInfo};

    fn snap(hwnd: Option<usize>, n: usize) -> SeeSnapshot {
        SeeSnapshot {
            window: hwnd.map(|h| WindowInfo {
                hwnd: h,
                title: String::new(),
                process: String::new(),
                pid: 0,
                rect: Rect::new(0, 0, 100, 100),
                is_foreground: false,
                is_minimized: false,
                is_sensitive: false,
            }),
            screen_rect: Rect::new(0, 0, 100, 100),
            mode_used: "uia".into(),
            elements: (0..n)
                .map(|i| UiElement {
                    id: i as u32 + 1,
                    role: "btn".into(),
                    name: format!("b{}", i),
                    value: None,
                    state: None,
                    rect: Rect::new(0, i as i32 * 20, 50, 18),
                    source: crate::types::ElementSource::Uia,
                    is_password: false,
                    is_interactive: true,
                    runtime_id: vec![],
                })
                .collect(),
            redacted_rects: vec![],
            sensitive_blocked: false,
            elapsed_ms: 0,
            coords: Default::default(),
            page_not_ready: false,
        }
    }

    #[test]
    fn per_window_baselines_survive_switching_and_cli_roundtrip() {
        let mut st = AppState::new(SafetyConfig::default());
        st.remember_snapshot(snap(Some(1), 3));
        st.remember_snapshot(snap(Some(2), 5));
        assert_eq!(st.snapshots.len(), 2);
        assert_eq!(st.last_snapshot.as_ref().unwrap().elements.len(), 5);

        let path =
            std::env::temp_dir().join(format!("eyra_state_test_{}.json", std::process::id()));
        st.save_snapshot_to_file(&path);
        let mut st2 = AppState::new(SafetyConfig::default());
        st2.load_snapshot_from_file(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(st2.snapshots.len(), 2);
        // The active snapshot (used by click/type #id) is preserved
        assert_eq!(st2.last_snapshot.unwrap().window.unwrap().hwnd, 2);
    }
}
