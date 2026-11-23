use crate::types::{CapturedFrame, Rect, SeeSnapshot, UiElement};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;

#[derive(Debug, Clone)]
pub struct SnapshotDiff {
    pub added: Vec<UiElement>,
    pub removed: Vec<UiElement>,
    pub changed: Vec<(UiElement, UiElement)>,
    pub unchanged_count: usize,
}

impl SnapshotDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn to_compact_text(&self, curr: &SeeSnapshot) -> String {
        let mut out = String::new();
        if let Some(win) = &curr.window {
            let _ = writeln!(
                out,
                "DIFF hwnd={} proc={} title=\"{}\" +{} ~{} -{} (unchanged={}) ms={}",
                win.hwnd_hex(),
                win.process,
                crate::types::truncate_clean(&win.title, 60),
                self.added.len(),
                self.changed.len(),
                self.removed.len(),
                self.unchanged_count,
                curr.elapsed_ms
            );
        } else {
            let _ = writeln!(
                out,
                "DIFF screen +{} ~{} -{} (unchanged={}) ms={}",
                self.added.len(),
                self.changed.len(),
                self.removed.len(),
                self.unchanged_count,
                curr.elapsed_ms
            );
        }

        if curr.sensitive_blocked {
            out.push_str("[SENSITIVE WINDOW REDACTED]\n");
            return out;
        }

        if self.is_empty() {
            out.push_str("(no UI changes since last see)\n");
            return out;
        }

        for el in &self.added {
            let _ = writeln!(out, "+ {}", el.to_compact_line_with(curr.coords));
        }
        for (_old, new_el) in &self.changed {
            let _ = writeln!(out, "~ {}", new_el.to_compact_line_with(curr.coords));
        }
        for el in &self.removed {
            let _ = writeln!(out, "- {}", el.to_compact_line_with(curr.coords));
        }

        out
    }
}

pub fn diff_snapshots(prev: &SeeSnapshot, curr: &SeeSnapshot) -> SnapshotDiff {
    // Match primarily by runtime_id when available, otherwise by role+name+quantized_pos
    let mut prev_by_rid: HashMap<&[i32], usize> = HashMap::new();
    let mut prev_by_fp: HashMap<String, Vec<usize>> = HashMap::new();

    for (idx, el) in prev.elements.iter().enumerate() {
        if !el.runtime_id.is_empty() {
            prev_by_rid.insert(&el.runtime_id, idx);
        }
        prev_by_fp.entry(el.fingerprint()).or_default().push(idx);
    }

    let mut matched_prev: HashSet<usize> = HashSet::new();
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged_count = 0usize;

    for el in &curr.elements {
        let mut found_idx: Option<usize> = None;

        if !el.runtime_id.is_empty() {
            if let Some(&idx) = prev_by_rid.get(el.runtime_id.as_slice()) {
                if !matched_prev.contains(&idx) {
                    found_idx = Some(idx);
                }
            }
        }

        if found_idx.is_none() {
            let fp = el.fingerprint();
            if let Some(list) = prev_by_fp.get(&fp) {
                for &idx in list {
                    if !matched_prev.contains(&idx) {
                        found_idx = Some(idx);
                        break;
                    }
                }
            }
        }

        if let Some(pidx) = found_idx {
            matched_prev.insert(pidx);
            let old = &prev.elements[pidx];
            if element_meaningfully_changed(old, el) {
                changed.push((old.clone(), el.clone()));
            } else {
                unchanged_count += 1;
            }
        } else {
            added.push(el.clone());
        }
    }

    let mut removed = Vec::new();
    for (idx, el) in prev.elements.iter().enumerate() {
        if !matched_prev.contains(&idx) {
            removed.push(el.clone());
        }
    }

    SnapshotDiff {
        added,
        removed,
        changed,
        unchanged_count,
    }
}

fn element_meaningfully_changed(a: &UiElement, b: &UiElement) -> bool {
    if a.role != b.role || a.name != b.name || a.value != b.value || a.state != b.state {
        return true;
    }
    let dx = (a.rect.x - b.rect.x).abs();
    let dy = (a.rect.y - b.rect.y).abs();
    let dw = (a.rect.w - b.rect.w).abs();
    let dh = (a.rect.h - b.rect.h).abs();
    dx > 6 || dy > 6 || dw > 6 || dh > 6
}

#[derive(Debug, Clone)]
pub struct PixelDiff {
    pub changed_percent: f32,
    pub changed_rect: Option<Rect>,
    pub changed_blocks: usize,
}

/// Computes block-level pixel difference between two frames of identical dimensions.
/// Returns changed percentage and a screen-space bounding rectangle covering all changed blocks.
pub fn diff_frames(prev: &CapturedFrame, curr: &CapturedFrame, pixel_threshold: u8) -> PixelDiff {
    if prev.width != curr.width || prev.height != curr.height || prev.width == 0 || prev.height == 0
    {
        return PixelDiff {
            changed_percent: 100.0,
            changed_rect: Some(curr.rect()),
            changed_blocks: 1,
        };
    }

    const BLOCK: u32 = 16;
    let w = curr.width;
    let h = curr.height;
    let bx_count = w.div_ceil(BLOCK);
    let by_count = h.div_ceil(BLOCK);
    let total_blocks = (bx_count as usize) * (by_count as usize);

    let thresh = pixel_threshold as i32;
    let mut changed_blocks = 0usize;
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x = 0u32;
    let mut max_y = 0u32;

    for by in 0..by_count {
        let y0 = by * BLOCK;
        let y1 = (y0 + BLOCK).min(h);
        for bx in 0..bx_count {
            let x0 = bx * BLOCK;
            let x1 = (x0 + BLOCK).min(w);

            let mut diff_pixels = 0usize;
            // Sample every 2nd pixel inside the 16x16 block for sub-millisecond performance
            let mut y = y0;
            while y < y1 {
                let row = (y as usize) * (w as usize) * 4;
                let mut x = x0;
                while x < x1 {
                    let idx = row + (x as usize) * 4;
                    let dr = (prev.rgba[idx] as i32 - curr.rgba[idx] as i32).abs();
                    let dg = (prev.rgba[idx + 1] as i32 - curr.rgba[idx + 1] as i32).abs();
                    let db = (prev.rgba[idx + 2] as i32 - curr.rgba[idx + 2] as i32).abs();
                    if dr > thresh || dg > thresh || db > thresh {
                        diff_pixels += 1;
                    }
                    x += 2;
                }
                y += 2;
            }

            // Require at least 2 sampled pixels in block to ignore 1px anti-aliasing noise
            if diff_pixels >= 2 {
                changed_blocks += 1;
                min_x = min_x.min(x0);
                min_y = min_y.min(y0);
                max_x = max_x.max(x1);
                max_y = max_y.max(y1);
            }
        }
    }

    let changed_percent = if total_blocks == 0 {
        0.0
    } else {
        (changed_blocks as f32 / total_blocks as f32) * 100.0
    };

    let changed_rect = if changed_blocks > 0 {
        let pad = 16i32;
        let left = (curr.origin_x + min_x as i32 - pad).max(curr.origin_x);
        let top = (curr.origin_y + min_y as i32 - pad).max(curr.origin_y);
        let right = (curr.origin_x + max_x as i32 + pad).min(curr.origin_x + w as i32);
        let bottom = (curr.origin_y + max_y as i32 + pad).min(curr.origin_y + h as i32);
        Some(Rect::from_ltrb(left, top, right, bottom))
    } else {
        None
    };

    PixelDiff {
        changed_percent,
        changed_rect,
        changed_blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ElementSource;

    fn make_el(id: u32, role: &str, name: &str, val: Option<&str>, x: i32) -> UiElement {
        UiElement {
            id,
            role: role.into(),
            name: name.into(),
            value: val.map(|s| s.into()),
            state: None,
            rect: Rect::new(x, 10, 80, 24),
            source: ElementSource::Uia,
            is_password: false,
            is_interactive: true,
            runtime_id: vec![1, id as i32],
        }
    }

    #[test]
    fn snapshot_diff_detects_add_change_remove() {
        let prev = SeeSnapshot {
            window: None,
            screen_rect: Rect::new(0, 0, 800, 600),
            mode_used: "uia".into(),
            elements: vec![
                make_el(1, "btn", "OK", None, 10),
                make_el(2, "inp", "Search", Some("old"), 100),
                make_el(3, "btn", "Cancel", None, 200),
            ],
            redacted_rects: vec![],
            sensitive_blocked: false,
            elapsed_ms: 5,
            coords: Default::default(),
            page_not_ready: false,
        };
        let curr = SeeSnapshot {
            window: None,
            screen_rect: Rect::new(0, 0, 800, 600),
            mode_used: "uia".into(),
            elements: vec![
                make_el(1, "btn", "OK", None, 10),
                make_el(2, "inp", "Search", Some("new query"), 100),
                make_el(4, "txt", "3 results", None, 300),
            ],
            redacted_rects: vec![],
            sensitive_blocked: false,
            elapsed_ms: 4,
            coords: Default::default(),
            page_not_ready: false,
        };

        let d = diff_snapshots(&prev, &curr);
        assert_eq!(d.unchanged_count, 1);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.removed.len(), 1);
        let text = d.to_compact_text(&curr);
        assert!(text.contains("+ #4 txt \"3 results\""));
        assert!(text.contains("~ #2 inp \"Search\" val=\"new query\""));
        assert!(text.contains("- #3 btn \"Cancel\""));
    }

    #[test]
    fn pixel_diff_isolates_changed_region() {
        let prev = CapturedFrame {
            width: 128,
            height: 128,
            origin_x: 100,
            origin_y: 50,
            rgba: vec![20u8; 128 * 128 * 4],
        };
        let mut curr = prev.clone();
        // Paint a 16x16 white patch at (64, 64)
        for y in 64..80 {
            for x in 64..80 {
                let idx = (y * 128 + x) * 4;
                curr.rgba[idx] = 250;
                curr.rgba[idx + 1] = 250;
                curr.rgba[idx + 2] = 250;
            }
        }
        let d = diff_frames(&prev, &curr, 18);
        assert_eq!(d.changed_blocks, 1);
        assert!(d.changed_percent > 1.0 && d.changed_percent < 3.0);
        let r = d.changed_rect.unwrap();
        assert_eq!(r, Rect::new(100 + 48, 50 + 48, 48, 48));
    }
}
