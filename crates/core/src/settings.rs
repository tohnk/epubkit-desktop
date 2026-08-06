//! Persisted settings: what the user last chose, and any presets they saved.
//!
//! # The model
//!
//! There is exactly one live set of option values, and a separate pointer to
//! which preset the UI should show as selected. The values are the truth; the
//! pointer is a label.
//!
//! That separation is what makes "restore what I had last time" a single rule
//! rather than three cases. On launch, [`Settings::options`] is restored
//! verbatim, whether the user last had a built-in preset selected, a preset
//! they saved, or something they tweaked by hand. [`Settings::active`] only
//! decides which chip lights up.
//!
//! Restoring by value rather than by name also means that if a built-in preset
//! is redefined in a later version, nobody's saved state silently changes under
//! them.
//!
//! # The rules
//!
//! - Selecting a preset copies its values into `options` and points `active` at
//!   it.
//! - Changing any option moves `active` to [`CUSTOM`], matching how the web UI
//!   already behaves. The difference here is that Custom persists and can be
//!   given a name.
//! - The device is *not* part of a preset. It describes the hardware on the
//!   desk, not a processing taste, so it is sticky on its own.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::image::{self, DeviceProfile};
use crate::metadata::MetadataEdits;
use crate::pipeline::ProcessingOptions;
use crate::{Error, Result};

/// Reserved `active` values. A saved preset may not use one as its id.
pub const QUICK: &str = "quick";
pub const FULL: &str = "full";
pub const CUSTOM: &str = "custom";

const RESERVED_IDS: &[&str] = &[QUICK, FULL, CUSTOM];

/// The processing choices a preset can hold.
///
/// Deliberately *not* including the device: see the module documentation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OptionSet {
    pub grayscale: bool,
    pub contrast_boost: bool,
    pub quality: u8,
    pub remove_fonts: bool,
    pub remove_unused_css: bool,
    pub light_novel_mode: bool,
    pub clean_metadata: bool,
    pub text_cleanup: bool,
}

impl Default for OptionSet {
    fn default() -> Self {
        Self::full()
    }
}

impl OptionSet {
    /// A fast pass: images and text, nothing structural.
    pub fn quick() -> Self {
        Self {
            grayscale: true,
            contrast_boost: true,
            quality: 70,
            remove_fonts: false,
            remove_unused_css: false,
            light_novel_mode: false,
            clean_metadata: false,
            text_cleanup: true,
        }
    }

    /// Everything the device benefits from.
    pub fn full() -> Self {
        Self {
            grayscale: true,
            contrast_boost: true,
            quality: 70,
            remove_fonts: true,
            remove_unused_css: true,
            light_novel_mode: false,
            clean_metadata: true,
            text_cleanup: true,
        }
    }

    /// Build the options the pipeline actually takes.
    pub fn to_processing_options(
        &self,
        device: DeviceProfile,
        edits: MetadataEdits,
    ) -> ProcessingOptions {
        ProcessingOptions {
            device,
            grayscale: self.grayscale,
            eink_quantize: self.grayscale,
            contrast_boost: self.contrast_boost,
            quality: self.quality,
            remove_fonts: self.remove_fonts,
            remove_unused_css: self.remove_unused_css,
            light_novel_mode: self.light_novel_mode,
            clean_metadata: self.clean_metadata,
            text_cleanup: self.text_cleanup,
            metadata_edits: edits,
            ..ProcessingOptions::default()
        }
    }
}

/// A named set of options the user chose to keep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub options: OptionSet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Which reader this machine is for. Sticky, and never part of a preset.
    pub device: String,
    /// The live values — restored verbatim on launch.
    pub options: OptionSet,
    /// Which preset the UI should show as selected. A label, not the truth.
    pub active: String,
    pub presets: Vec<Preset>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            device: image::DEFAULT_DEVICE.to_string(),
            options: OptionSet::full(),
            active: FULL.to_string(),
            presets: Vec::new(),
        }
    }
}

impl Settings {
    /// Where settings live on this platform.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("epubkit").join("settings.toml"))
    }

    /// Read settings, falling back to defaults when the file does not exist.
    ///
    /// A file that exists but cannot be parsed is an error rather than a silent
    /// reset — quietly discarding someone's saved presets is worse than saying
    /// the file is broken.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        toml::from_str(&text).map_err(|e| Error::Settings(format!("{}: {e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Settings(format!("could not serialize settings: {e}")))?;
        std::fs::write(path, text).map_err(|e| Error::io(path, e))
    }

    /// The device profile, falling back to the default if the stored id is not
    /// one this build knows about.
    pub fn device_profile(&self) -> DeviceProfile {
        image::device(&self.device)
            .or_else(|| image::device(image::DEFAULT_DEVICE))
            .unwrap_or(image::X4)
    }

    /// Apply a preset: copy its values in and point `active` at it.
    pub fn select(&mut self, id: &str) -> Result<()> {
        let options = match id {
            QUICK => OptionSet::quick(),
            FULL => OptionSet::full(),
            // Selecting Custom means "keep what is on screen"; there are no
            // canonical values to restore.
            CUSTOM => self.options.clone(),
            _ => self
                .presets
                .iter()
                .find(|preset| preset.id == id)
                .map(|preset| preset.options.clone())
                .ok_or_else(|| Error::Settings(format!("no preset named '{id}'")))?,
        };

        self.options = options;
        self.active = id.to_string();
        Ok(())
    }

    /// Record that the user changed something by hand.
    ///
    /// Call this after mutating [`Settings::options`] directly. It moves the
    /// selection to Custom, so the UI stops claiming a preset is in effect when
    /// it no longer is.
    pub fn mark_customized(&mut self) {
        self.active = CUSTOM.to_string();
    }

    /// Promote the current options into a named preset and select it.
    pub fn save_preset(&mut self, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Settings("a preset needs a name".into()));
        }

        let id = self.unique_id(&slugify(name));
        self.presets.push(Preset {
            id: id.clone(),
            name: name.to_string(),
            options: self.options.clone(),
        });
        self.active = id.clone();

        Ok(id)
    }

    /// Overwrite an existing preset with the current options.
    pub fn update_preset(&mut self, id: &str) -> Result<()> {
        let options = self.options.clone();
        let preset = self
            .presets
            .iter_mut()
            .find(|preset| preset.id == id)
            .ok_or_else(|| Error::Settings(format!("no preset named '{id}'")))?;

        preset.options = options;
        self.active = id.to_string();
        Ok(())
    }

    /// Remove a saved preset. The live options are left alone — deleting a
    /// preset should not change how the next book is processed.
    pub fn delete_preset(&mut self, id: &str) -> Result<()> {
        let before = self.presets.len();
        self.presets.retain(|preset| preset.id != id);

        if self.presets.len() == before {
            return Err(Error::Settings(format!("no preset named '{id}'")));
        }

        if self.active == id {
            self.active = CUSTOM.to_string();
        }
        Ok(())
    }

    /// Find a preset by id.
    pub fn preset(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.id == id)
    }

    /// The label the UI should show for the current selection.
    pub fn active_label(&self) -> String {
        match self.active.as_str() {
            QUICK => "Quick".to_string(),
            FULL => "Full".to_string(),
            CUSTOM => "Custom".to_string(),
            id => self
                .preset(id)
                .map(|preset| preset.name.clone())
                // A preset deleted out from under the pointer reads as Custom
                // rather than as a dangling name.
                .unwrap_or_else(|| "Custom".to_string()),
        }
    }

    fn unique_id(&self, base: &str) -> String {
        let base = if base.is_empty() { "preset" } else { base };

        let taken = |candidate: &str| {
            RESERVED_IDS.contains(&candidate)
                || self.presets.iter().any(|preset| preset.id == candidate)
        };

        if !taken(base) {
            return base.to_string();
        }

        (2..)
            .map(|n| format!("{base}-{n}"))
            .find(|candidate| !taken(candidate))
            .expect("an unused suffix always exists")
    }
}

/// Turn a display name into something safe to use as an id and a TOML key.
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut last_was_dash = true; // suppresses a leading dash

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    slug.trim_end_matches('-').to_string()
}
