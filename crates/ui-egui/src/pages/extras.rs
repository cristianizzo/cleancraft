//! Per-module controls shown above a plan's results (and on the idle page), dispatched to
//! files owned by each module's builder.

use cleancraft_core::FeatureId;
use egui::Ui;

use crate::app::CleanApp;

/// Shown on the idle page above the Scan button (e.g. a folder picker).
pub fn before_scan(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    match f {
        FeatureId::TrashBins
        | FeatureId::MailAttachments
        | FeatureId::Privacy
        | FeatureId::SystemJunk => super::extras_cleanup::before_scan(app, ui, f),
        FeatureId::LargeOldFiles | FeatureId::Duplicates | FeatureId::SimilarPhotos => {
            super::extras_files::before_scan(app, ui, f)
        }
        FeatureId::Malware => super::extras_files::threat_depth_picker(app, ui),
        _ => {}
    }
}

/// Shown in the results toolbar.
pub fn results_toolbar(app: &mut CleanApp, ui: &mut Ui, f: FeatureId) {
    match f {
        FeatureId::TrashBins
        | FeatureId::MailAttachments
        | FeatureId::Privacy
        | FeatureId::SystemJunk => super::extras_cleanup::results_toolbar(app, ui, f),
        FeatureId::LargeOldFiles | FeatureId::Duplicates | FeatureId::SimilarPhotos => {
            super::extras_files::results_toolbar(app, ui, f)
        }
        _ => {}
    }
}
