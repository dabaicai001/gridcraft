//! The membership marker: a slanted blue "V" over the controls whose feature WPS puts behind a
//! membership — the Data tab's paid-tier tools, the Fill menu's batch editors and the filter
//! list's Duplicates / Unique Values / Fuzzy Filter. GridCraft ships every one of them free, so
//! the badge only says "paid elsewhere"; it never gates anything here.

use egui::{Color32, Rect, Ui, pos2, vec2};

use crate::icons::{self, Icon};

/// The commands, and the dialogs that run them, whose feature WPS puts behind a membership.
/// Kept next to the features they mark; `xtask`-free, plain data.
const VIP: &[&str] = &[
    "data.quickFilter",
    "data.compareRanges",
    "dialog:compareRanges",
    "data.splitByColumn",
    "dialog:splitByColumn",
    "data.mergeSheets",
    "dialog:mergeSheets",
    "data.conditionalStatistics",
    "dialog:conditionalStatistics",
    "data.rankEntry",
    "data.createIndex",
    "edit.fillBlanks",
    "dialog:fillBlanks",
    "edit.insertTextBatch",
    "dialog:insertTextBatch",
    "selection.invert",
    "insert.pictures",
    "data.filterDuplicates",
    "data.filterUnique",
    "data.filterFuzzy",
    "dialog:fuzzyFilter",
];

/// True when the control running `id` carries the marker.
pub fn is_vip(id: &str) -> bool {
    VIP.contains(&id)
}

/// Paints the marker over `rect`'s top-right corner.
pub fn paint(ui: &Ui, rect: Rect) {
    let side = 13.0;
    let at = Rect::from_center_size(pos2(rect.right() - side * 0.70, rect.top() + side * 0.55), vec2(side, side));
    icons::paint(ui.painter(), at, Icon::Vip, Color32::WHITE);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_covers_the_membership_tier_features_and_nothing_else() {
        for id in ["data.quickFilter", "dialog:compareRanges", "dialog:fillBlanks", "data.filterFuzzy"] {
            assert!(is_vip(id), "{id}");
        }
        for id in ["home.bold", "data.filter", "data.sortAscending", "edit.undo", "data.filterBy"] {
            assert!(!is_vip(id), "{id}");
        }
    }

    #[test]
    fn every_marked_command_exists_and_the_list_has_no_duplicates() {
        let specs = gridcraft_engine::command_specs();
        let mut seen: Vec<&str> = Vec::new();
        for id in VIP {
            assert!(!seen.contains(id), "duplicate entry: {id}");
            seen.push(id);
            // Dialog ids name a dialog in `crate::dialogs`; the rest must be real commands.
            if let Some(cmd) = id.strip_prefix("dialog:") {
                assert!(!cmd.is_empty());
            } else {
                assert!(specs.iter().any(|c| c.id == *id), "no such command: {id}");
            }
        }
    }
}
