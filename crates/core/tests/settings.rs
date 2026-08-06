use epubkit_core::settings::{OptionSet, Settings, CUSTOM, FULL, QUICK};

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn defaults_to_the_full_preset() {
    let settings = Settings::default();
    assert_eq!(settings.active, FULL);
    assert_eq!(settings.options, OptionSet::full());
    assert_eq!(settings.device, "x4");
}

#[test]
fn quick_and_full_differ_in_the_structural_steps() {
    let quick = OptionSet::quick();
    let full = OptionSet::full();

    assert!(quick.grayscale && full.grayscale);
    assert!(quick.text_cleanup && full.text_cleanup);
    assert!(!quick.remove_fonts && full.remove_fonts);
    assert!(!quick.remove_unused_css && full.remove_unused_css);
    assert!(!quick.clean_metadata && full.clean_metadata);
}

#[test]
fn selecting_a_preset_copies_its_values_in() {
    let mut settings = Settings::default();
    settings.select(QUICK).unwrap();

    assert_eq!(settings.active, QUICK);
    assert_eq!(settings.options, OptionSet::quick());
}

#[test]
fn changing_an_option_moves_the_selection_to_custom() {
    let mut settings = Settings::default();
    settings.options.light_novel_mode = true;
    settings.mark_customized();

    assert_eq!(settings.active, CUSTOM);
    assert!(settings.options.light_novel_mode);
}

/// The whole point of the model: what comes back is the values that were on
/// screen, not whatever the named preset happens to mean today.
#[test]
fn a_tweaked_preset_restores_the_tweak_not_the_preset() {
    let dir = tempdir();
    let path = dir.path().join("settings.toml");

    let mut settings = Settings::default();
    settings.select(FULL).unwrap();
    // The thing the user always turns off.
    settings.options.clean_metadata = false;
    settings.mark_customized();
    settings.save(&path).unwrap();

    let restored = Settings::load(&path).unwrap();
    assert!(
        !restored.options.clean_metadata,
        "the tweak should have survived the round-trip"
    );
    assert_eq!(restored.active, CUSTOM);
}

#[test]
fn an_untouched_preset_restores_as_that_preset() {
    let dir = tempdir();
    let path = dir.path().join("settings.toml");

    let mut settings = Settings::default();
    settings.select(QUICK).unwrap();
    settings.save(&path).unwrap();

    let restored = Settings::load(&path).unwrap();
    assert_eq!(restored.active, QUICK);
    assert_eq!(restored.options, OptionSet::quick());
}

#[test]
fn a_saved_preset_restores_as_itself() {
    let dir = tempdir();
    let path = dir.path().join("settings.toml");

    let mut settings = Settings::default();
    settings.options.quality = 85;
    settings.options.clean_metadata = false;
    let id = settings.save_preset("My X4").unwrap();
    settings.save(&path).unwrap();

    let restored = Settings::load(&path).unwrap();
    assert_eq!(restored.active, id);
    assert_eq!(restored.active_label(), "My X4");
    assert_eq!(restored.options.quality, 85);
    assert!(!restored.options.clean_metadata);
}

/// Restoring by value rather than by name means a change to what "Full" is
/// defined as cannot rewrite someone's stored choices.
#[test]
fn stored_values_do_not_track_a_redefined_preset() {
    let dir = tempdir();
    let path = dir.path().join("settings.toml");

    let mut settings = Settings::default();
    settings.select(FULL).unwrap();
    settings.save(&path).unwrap();

    // Whatever `full()` means later, the file holds concrete values.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("remove_fonts = true"), "{text}");
    assert!(text.contains("[options]"), "{text}");
}

#[test]
fn saving_a_preset_selects_it() {
    let mut settings = Settings::default();
    settings.options.quality = 40;

    let id = settings.save_preset("Low quality").unwrap();

    assert_eq!(id, "low-quality");
    assert_eq!(settings.active, id);
    assert_eq!(settings.preset(&id).unwrap().options.quality, 40);
}

#[test]
fn preset_names_become_readable_ids() {
    let mut settings = Settings::default();
    assert_eq!(
        settings.save_preset("My X4 Settings!").unwrap(),
        "my-x4-settings"
    );
    assert_eq!(
        settings.save_preset("  Spaced  Out  ").unwrap(),
        "spaced-out"
    );
}

#[test]
fn duplicate_preset_names_get_distinct_ids() {
    let mut settings = Settings::default();
    let first = settings.save_preset("Reading").unwrap();
    let second = settings.save_preset("Reading").unwrap();

    assert_eq!(first, "reading");
    assert_eq!(second, "reading-2");
    assert_eq!(settings.presets.len(), 2);
}

/// A preset called "Custom" must not collide with the reserved selection value,
/// or selecting it would silently mean something else.
#[test]
fn preset_ids_cannot_shadow_the_reserved_names() {
    let mut settings = Settings::default();

    for reserved in ["Quick", "Full", "Custom"] {
        let id = settings.save_preset(reserved).unwrap();
        assert_ne!(
            id,
            reserved.to_lowercase(),
            "{reserved} was allowed to collide"
        );
    }
}

#[test]
fn a_nameless_preset_is_refused() {
    let mut settings = Settings::default();
    assert!(settings.save_preset("   ").is_err());
    assert!(settings.presets.is_empty());
}

#[test]
fn updating_a_preset_overwrites_its_values() {
    let mut settings = Settings::default();
    settings.options.quality = 50;
    let id = settings.save_preset("Mine").unwrap();

    settings.options.quality = 90;
    settings.update_preset(&id).unwrap();

    assert_eq!(settings.preset(&id).unwrap().options.quality, 90);
    assert_eq!(settings.active, id);
}

#[test]
fn deleting_a_preset_leaves_the_live_options_alone() {
    let mut settings = Settings::default();
    settings.options.quality = 33;
    let id = settings.save_preset("Doomed").unwrap();

    settings.delete_preset(&id).unwrap();

    assert!(settings.presets.is_empty());
    assert_eq!(settings.active, CUSTOM, "the pointer should not dangle");
    assert_eq!(
        settings.options.quality, 33,
        "deleting a preset should not change how the next book is processed"
    );
}

#[test]
fn operating_on_an_unknown_preset_is_an_error() {
    let mut settings = Settings::default();
    assert!(settings.select("nope").is_err());
    assert!(settings.update_preset("nope").is_err());
    assert!(settings.delete_preset("nope").is_err());
}

/// The device describes the hardware on the desk, not a processing taste, so
/// switching presets must not move it.
#[test]
fn the_device_is_independent_of_presets() {
    let mut settings = Settings {
        device: "x3".to_string(),
        ..Settings::default()
    };

    settings.select(QUICK).unwrap();
    assert_eq!(settings.device, "x3");

    settings.save_preset("Whatever").unwrap();
    assert_eq!(settings.device, "x3");

    let text = toml::to_string_pretty(&settings).unwrap();
    assert!(text.contains(r#"device = "x3""#), "{text}");
    assert!(
        !text.contains("device") || text.matches("device").count() == 1,
        "the device should appear once, outside any preset:\n{text}"
    );
}

#[test]
fn an_unknown_device_falls_back_rather_than_failing() {
    let settings = Settings {
        device: "x9-from-the-future".to_string(),
        ..Settings::default()
    };

    assert_eq!(settings.device_profile().id, "x4");
}

#[test]
fn a_missing_file_gives_defaults() {
    let dir = tempdir();
    let settings = Settings::load(&dir.path().join("nothing-here.toml")).unwrap();
    assert_eq!(settings, Settings::default());
}

/// Silently resetting to defaults would throw away someone's saved presets
/// without telling them.
#[test]
fn a_corrupt_file_is_reported_not_ignored() {
    let dir = tempdir();
    let path = dir.path().join("settings.toml");
    std::fs::write(&path, "this is not toml { { {").unwrap();

    assert!(Settings::load(&path).is_err());
}

/// A file written by an older version, missing fields added since, should still
/// load — with defaults filling the gaps.
#[test]
fn a_partial_file_loads_with_defaults_for_the_rest() {
    let dir = tempdir();
    let path = dir.path().join("settings.toml");
    std::fs::write(&path, "device = \"x3\"\n\n[options]\nquality = 42\n").unwrap();

    let settings = Settings::load(&path).unwrap();
    assert_eq!(settings.device, "x3");
    assert_eq!(settings.options.quality, 42);
    assert_eq!(
        settings.options.remove_fonts,
        OptionSet::default().remove_fonts
    );
}

#[test]
fn saving_creates_the_directory() {
    let dir = tempdir();
    let path = dir
        .path()
        .join("nested")
        .join("deeper")
        .join("settings.toml");

    Settings::default().save(&path).unwrap();
    assert!(path.exists());
}

#[test]
fn labels_read_the_way_the_ui_shows_them() {
    let mut settings = Settings::default();
    assert_eq!(settings.active_label(), "Full");

    settings.select(QUICK).unwrap();
    assert_eq!(settings.active_label(), "Quick");

    settings.mark_customized();
    assert_eq!(settings.active_label(), "Custom");

    settings.save_preset("Night Reading").unwrap();
    assert_eq!(settings.active_label(), "Night Reading");
}

#[test]
fn options_convert_into_what_the_pipeline_takes() {
    let settings = Settings::default();
    let options = settings.options.to_processing_options(
        settings.device_profile(),
        epubkit_core::metadata::MetadataEdits::default(),
    );

    assert_eq!(options.device.id, "x4");
    assert_eq!(options.quality, settings.options.quality);
    assert_eq!(options.remove_fonts, settings.options.remove_fonts);
    assert_eq!(
        options.eink_quantize, settings.options.grayscale,
        "quantizing only makes sense when converting to grey"
    );
}

#[test]
fn the_written_file_is_readable_by_a_human() {
    let mut settings = Settings {
        device: "x3".to_string(),
        ..Settings::default()
    };
    settings.options.clean_metadata = false;
    settings.save_preset("My X3").unwrap();

    let text = toml::to_string_pretty(&settings).unwrap();

    assert!(text.contains("[options]"), "{text}");
    assert!(text.contains("[[presets]]"), "{text}");
    assert!(text.contains(r#"name = "My X3""#), "{text}");
    // And it round-trips.
    let parsed: Settings = toml::from_str(&text).unwrap();
    assert_eq!(parsed, settings);
}
