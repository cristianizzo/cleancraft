//! Pages. Plan-based modules share [`plan_page`]; the others have their own file.

pub mod apps;
pub mod dev_clutter;
pub mod disk_map;
pub mod extras;
pub mod extras_cleanup;
pub mod extras_files;
pub mod guide;
pub mod health;
pub mod history;
pub mod monitor;
pub mod plan_page;
pub mod settings;
pub mod startup;
pub mod sweep;
pub mod tasks;
pub mod threat_rules;
pub mod updates;
pub mod welcome;

use cleancraft_core::FeatureId;
use egui::Ui;

use crate::app::CleanApp;

/// What a module's scan looks at, for its start screen (one short line each, our own words).
pub fn covers(f: FeatureId) -> &'static [&'static str] {
    match f {
        FeatureId::SmartCare => &[
            "Junk files of macOS and your apps",
            "Attachments Mail downloaded",
            "What waits in the Trash",
            "Threats and unwanted software",
            "Updates for your apps",
        ],
        FeatureId::SystemJunk => &[
            "Caches and logs of macOS and your apps",
            "Crash reports and broken preferences",
            "Old app updates and unused disk images",
            "Xcode, simulator and developer caches",
            "Old iPhone and iPad backups and updates",
        ],
        FeatureId::MailAttachments => &[
            "Copies of attachments Mail saved in Mail Downloads",
            "Your messages and their attachments stay in Mail",
        ],
        FeatureId::TrashBins => &[
            "The Trash of this Mac's disk",
            "The Trash of each external disk",
            "Messages you deleted in Mail",
        ],
        FeatureId::Malware => &[
            "Apps and the programs they start",
            "Startup items and old-style startup items",
            "Known unwanted software, by CleanCraft's rules",
        ],
        FeatureId::Privacy => &[
            "Browser history, cookies and caches",
            "Lists of recently opened documents and apps",
            "Saved Wi-Fi networks",
            "Permissions you gave to apps",
        ],
        FeatureId::LargeOldFiles => &[
            "Files over your size limit",
            "Files you have not opened for a long time",
            "The folder you choose",
        ],
        FeatureId::Duplicates => &[
            "Files with exactly the same content",
            "One copy of each set always stays",
        ],
        FeatureId::SimilarPhotos => &[
            "Pictures that look alike",
            "One picture of each set always stays",
        ],
        FeatureId::Updater => &[
            "Apps that update through their own feed",
            "Apps from the App Store",
            "macOS itself",
        ],
        FeatureId::SpaceLens => &[
            "Every folder of your home folder, by size",
            "Any folder you pick",
        ],
        FeatureId::DevClutter => &[
            "Rust target folders, node_modules and Python environments",
            "Selected only in projects you have not changed for a while",
            "Xcode simulators that can no longer start",
            "What Docker and Homebrew could free with their own commands",
        ],
        _ => &[],
    }
}

pub fn feature_page(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    match f {
        FeatureId::SmartCare => sweep::ui(app, ui, f),
        FeatureId::Uninstaller | FeatureId::Updater => apps::ui(app, ui, f),
        FeatureId::LoginItems => startup::ui(app, ui, f),
        FeatureId::Maintenance => tasks::ui(app, ui, f),
        FeatureId::SpaceLens => disk_map::ui(app, ui, f),
        FeatureId::Monitor => monitor::ui(app, ui, f),
        FeatureId::DevClutter => dev_clutter::ui(app, ui, f),
        _ => plan_page::ui(app, ui, f),
    }
}
