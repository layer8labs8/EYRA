use serde::{Deserialize, Serialize};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn from_ltrb(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        Self {
            x: left,
            y: top,
            w: (right - left).max(0),
            h: (bottom - top).max(0),
        }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn area(&self) -> i64 {
        (self.w.max(0) as i64) * (self.h.max(0) as i64)
    }

    pub fn is_valid(&self) -> bool {
        self.w > 0 && self.h > 0
    }

    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.right()
            && self.right() > other.x
            && self.y < other.bottom()
            && self.bottom() > other.y
    }

    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right > left && bottom > top {
            Some(Rect::from_ltrb(left, top, right, bottom))
        } else {
            None
        }
    }

    pub fn iou(&self, other: &Rect) -> f32 {
        let Some(inter) = self.intersection(other) else {
            return 0.0;
        };
        let inter_area = inter.area() as f64;
        let union_area = (self.area() + other.area() - inter.area()) as f64;
        if union_area <= 0.0 {
            0.0
        } else {
            (inter_area / union_area) as f32
        }
    }

    pub fn overlap_ratio_of_self(&self, other: &Rect) -> f32 {
        let Some(inter) = self.intersection(other) else {
            return 0.0;
        };
        let a = self.area() as f64;
        if a <= 0.0 {
            0.0
        } else {
            (inter.area() as f64 / a) as f32
        }
    }

    pub fn clamp_to(&self, bounds: &Rect) -> Option<Rect> {
        self.intersection(bounds)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub hwnd: usize,
    pub title: String,
    pub process: String,
    pub pid: u32,
    pub rect: Rect,
    pub is_foreground: bool,
    pub is_minimized: bool,
    pub is_sensitive: bool,
}

impl WindowInfo {
    pub fn hwnd_hex(&self) -> String {
        format!("0x{:X}", self.hwnd)
    }

    pub fn to_compact_line(&self) -> String {
        let mut flags = String::new();
        if self.is_foreground {
            flags.push_str(" [FG]");
        }
        if self.is_minimized {
            flags.push_str(" [MIN]");
        }
        if self.is_sensitive {
            flags.push_str(" [SENSITIVE]");
        }
        format!(
            "{} | {} | \"{}\" | {},{},{},{}{}",
            self.hwnd_hex(),
            self.process,
            truncate_clean(&self.title, 72),
            self.rect.x,
            self.rect.y,
            self.rect.w,
            self.rect.h,
            flags
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ElementSource {
    Uia,
    Ocr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    pub id: u32,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub rect: Rect,
    pub source: ElementSource,
    pub is_password: bool,
    pub is_interactive: bool,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub runtime_id: Vec<i32>,
}

impl UiElement {
    #[cfg(test)]
    pub fn to_compact_line(&self) -> String {
        self.to_compact_line_with(CoordStyle::Rect)
    }

    pub fn to_compact_line_with(&self, coords: CoordStyle) -> String {
        let mut out = format!(
            "#{} {} \"{}\"",
            self.id,
            self.role,
            truncate_clean(&self.name, 80)
        );
        // Hyperlink "values" are their hrefs (often 60+ chars each on web pages); agents act by
        // #id, so they are dropped from the compact line. Values that repeat the name are noise.
        let show_val = self.role != "lnk" || self.is_password;
        if let Some(val) = self.value.as_ref().filter(|_| show_val) {
            if !val.is_empty() && val.trim() != self.name {
                let display_val = if self.is_password {
                    "[REDACTED]".to_string()
                } else {
                    truncate_clean(val, 60)
                };
                let _ = write!(out, " val=\"{}\"", display_val);
            }
        }
        if let Some(st) = &self.state {
            if !st.is_empty() {
                let _ = write!(out, " ({})", st);
            }
        }
        match coords {
            CoordStyle::Rect => {
                let _ = write!(
                    out,
                    " [{},{},{},{}]",
                    self.rect.x, self.rect.y, self.rect.w, self.rect.h
                );
            }
            CoordStyle::Center => {
                let (cx, cy) = self.rect.center();
                let _ = write!(out, " @{},{}", cx, cy);
            }
        }
        out
    }

    pub fn fingerprint(&self) -> String {
        // Quantize position to 8px buckets to absorb minor layout jitter
        let qx = self.rect.x / 8;
        let qy = self.rect.y / 8;
        format!("{}|{}|{},{}", self.role, self.name, qx, qy)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeeMode {
    Auto,
    Uia,
    Ocr,
    Both,
}

impl SeeMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "uia" => Self::Uia,
            "ocr" => Self::Ocr,
            "both" | "hybrid" => Self::Both,
            _ => Self::Auto,
        }
    }
}

/// How element positions are printed in the compact map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoordStyle {
    /// `[x,y,w,h]` (default)
    #[default]
    Rect,
    /// `@cx,cy`: the click point only, roughly a third fewer tokens on dense pages
    Center,
}

impl CoordStyle {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "center" | "centre" | "point" => Self::Center,
            _ => Self::Rect,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeeSnapshot {
    pub window: Option<WindowInfo>,
    pub screen_rect: Rect,
    pub mode_used: String,
    pub elements: Vec<UiElement>,
    pub redacted_rects: Vec<Rect>,
    pub sensitive_blocked: bool,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub coords: CoordStyle,
}

impl SeeSnapshot {
    pub fn to_compact_text(&self) -> String {
        let mut out = String::new();
        if let Some(win) = &self.window {
            let _ = writeln!(
                out,
                "WIN hwnd={} proc={} title=\"{}\" rect={},{},{},{} mode={} ms={}",
                win.hwnd_hex(),
                win.process,
                truncate_clean(&win.title, 80),
                win.rect.x,
                win.rect.y,
                win.rect.w,
                win.rect.h,
                self.mode_used,
                self.elapsed_ms
            );
        } else {
            let _ = writeln!(
                out,
                "SCREEN rect={},{},{},{} mode={} ms={}",
                self.screen_rect.x,
                self.screen_rect.y,
                self.screen_rect.w,
                self.screen_rect.h,
                self.mode_used,
                self.elapsed_ms
            );
        }

        if self.sensitive_blocked {
            out.push_str("[SENSITIVE WINDOW REDACTED: contents hidden by safety policy]\n");
            return out;
        }

        if self.mode_used == "minimized" {
            out.push_str("[WINDOW MINIMIZED: restore it with focus_window(window) first]\n");
            return out;
        }

        if self.elements.is_empty() {
            out.push_str("(no visible elements detected; try mode=\"ocr\" or screenshot)\n");
            return out;
        }

        for el in &self.elements {
            out.push_str(&el.to_compact_line_with(self.coords));
            out.push('\n');
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub rgba: Vec<u8>,
}

impl CapturedFrame {
    pub fn rect(&self) -> Rect {
        Rect::new(
            self.origin_x,
            self.origin_y,
            self.width as i32,
            self.height as i32,
        )
    }
}

pub fn truncate_clean(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            '"' => '\'',
            c if c.is_control() => ' ',
            other => other,
        })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= max_chars {
        collapsed
    } else {
        let mut res: String = collapsed
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        res.push('…');
        res
    }
}

/// Approximate text token count (~4 characters per token for structured ASCII/UTF-8).
pub fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        0
    } else {
        chars.div_ceil(4)
    }
}

/// Approximate vision token cost for a raw image (Anthropic/modern VLM tile heuristic: (w*h)/750).
pub fn estimate_vision_tokens(width: u32, height: u32) -> usize {
    let pixels = (width as u64) * (height as u64);
    pixels.div_ceil(750).max(85) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_geometry_works() {
        let a = Rect::new(10, 20, 100, 50);
        let b = Rect::new(60, 20, 100, 50);
        assert_eq!(a.center(), (60, 45));
        assert!(a.intersects(&b));
        let inter = a.intersection(&b).unwrap();
        assert_eq!(inter, Rect::new(60, 20, 50, 50));
        assert!((a.iou(&b) - (2500.0 / 7500.0)).abs() < 1e-4);
    }

    #[test]
    fn password_element_is_redacted_in_compact_line() {
        let el = UiElement {
            id: 3,
            role: "pwd".into(),
            name: "Password".into(),
            value: Some("super-secret-123".into()),
            state: None,
            rect: Rect::new(100, 200, 220, 32),
            source: ElementSource::Uia,
            is_password: true,
            is_interactive: true,
            runtime_id: vec![42, 1],
        };
        let line = el.to_compact_line();
        assert!(line.contains("[REDACTED]"));
        assert!(!line.contains("super-secret-123"));
    }

    #[test]
    fn token_estimation_is_sensible() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert!(estimate_vision_tokens(1920, 1080) > 2500);
    }

    #[test]
    fn link_href_and_name_echo_values_are_omitted() {
        let mut el = UiElement {
            id: 7,
            role: "lnk".into(),
            name: "Pull requests".into(),
            value: Some("https://github.com/org/repo/pulls".into()),
            state: None,
            rect: Rect::new(10, 20, 80, 16),
            source: ElementSource::Uia,
            is_password: false,
            is_interactive: true,
            runtime_id: vec![],
        };
        assert!(!el.to_compact_line().contains("val="));

        el.role = "inp".into();
        el.value = Some("Pull requests".into());
        assert!(
            !el.to_compact_line().contains("val="),
            "value equal to name is noise"
        );

        el.value = Some("typed text".into());
        assert!(el.to_compact_line().contains("val=\"typed text\""));
    }

    #[test]
    fn center_coords_print_click_point_only() {
        let el = UiElement {
            id: 4,
            role: "btn".into(),
            name: "Save".into(),
            value: None,
            state: None,
            rect: Rect::new(100, 200, 80, 30),
            source: ElementSource::Uia,
            is_password: false,
            is_interactive: true,
            runtime_id: vec![],
        };
        assert_eq!(
            el.to_compact_line_with(CoordStyle::Center),
            "#4 btn \"Save\" @140,215"
        );
        assert_eq!(
            el.to_compact_line_with(CoordStyle::Rect),
            "#4 btn \"Save\" [100,200,80,30]"
        );
        assert_eq!(CoordStyle::parse("CENTER"), CoordStyle::Center);
        assert_eq!(CoordStyle::parse("anything"), CoordStyle::Rect);
    }

    #[test]
    fn minimized_window_snapshot_says_so_instead_of_listing_junk() {
        let snap = SeeSnapshot {
            window: None,
            screen_rect: Rect::new(-32000, -32000, 160, 28),
            mode_used: "minimized".into(),
            elements: Vec::new(),
            redacted_rects: Vec::new(),
            sensitive_blocked: false,
            elapsed_ms: 1,
            coords: CoordStyle::Rect,
        };
        let text = snap.to_compact_text();
        assert!(text.contains("WINDOW MINIMIZED"));
        assert!(text.contains("focus_window"));
    }
}
