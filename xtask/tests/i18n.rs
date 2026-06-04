// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Fixture-driven end-to-end tests for the localization tooling.
//!
//! These drive `extract`/`check`/`compile` against a throwaway workspace built
//! under `CARGO_TARGET_TMPDIR`, exercising real `xgettext`/`msgfmt`/… runs:
//! call-shape extraction, contexts, plurals, rust-format flags, catalog
//! validation, header integrity, the named-placeholder rule, and clean
//! recompilation. They require GNU gettext >= 0.24 (older releases lack
//! rust-format) and are therefore `#[ignore]`d for the default `cargo test`
//! run on environments without it; the CI i18n job runs them with
//! `--include-ignored` on an image that ships gettext 0.26.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::workspace::APP_ID;

/// A throwaway workspace whose layout matches what the tooling expects.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// Build a standard, valid fixture (good call shapes, a desktop entry, and
    /// AppStream metadata) under a per-test directory.
    fn standard(name: &str) -> Fixture {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        let fixture = Fixture { root };

        // Presentation-boundary crate: every supported call shape, including a
        // plural nested in formatx! and a contextual plural.
        fixture.write(
            "crates/ui_gtk/src/lib.rs",
            r#"use sustain_i18n::{formatx, gettext, ngettext, npgettext, pgettext};
pub fn ui(n: u64) -> String {
    let _ = gettext("Songs");
    let _ = pgettext("column header", "Play");
    let _ = npgettext("queue", "{count} album", "{count} albums", n);
    formatx!(ngettext("{count} song imported", "{count} songs imported", n), count = n).unwrap()
}
"#,
        );
        fixture.write(
            "crates/app_runtime/src/lib.rs",
            "use sustain_i18n::gettext;\npub fn note() -> String { gettext(\"Scanning library\") }\n",
        );
        // Durable crate: string-free, so nothing is extracted from it.
        fixture.write(
            "crates/domain/src/lib.rs",
            "pub fn answer() -> i32 { 42 }\n",
        );
        // The i18n crate is the one place allowed to name gettextrs.
        fixture.write("crates/i18n/src/lib.rs", "pub use gettextrs::gettext;\n");

        fixture.write(
            &format!("data/{APP_ID}.desktop"),
            "[Desktop Entry]\nType=Application\nName=Sustain\nComment=Fixture comment\nExec=sustain\n",
        );
        fixture.write(
            &format!("data/{APP_ID}.metainfo.xml"),
            &format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <component type=\"desktop-application\">\n\
                 \x20 <id>{APP_ID}</id>\n\
                 \x20 <name>Sustain</name>\n\
                 \x20 <summary>Fixture summary</summary>\n\
                 \x20 <description><p>Fixture description paragraph.</p></description>\n\
                 </component>\n"
            ),
        );
        fixture.write("po/LINGUAS", "# fixture\n");
        fixture.write("po/GLOSSARY.md", "# fixture glossary\n");
        fixture
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("relative path has a parent"))
            .expect("create fixture dir");
        std::fs::write(&path, contents).expect("write fixture file");
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.root.join(relative)).expect("read fixture file")
    }

    fn pot(&self) -> String {
        self.read("po/sustain.pot")
    }

    /// Generate a complete, non-fuzzy `fr` catalog from the current template
    /// (msgstr = msgid for every entry) and list it in LINGUAS.
    fn add_complete_fr_catalog(&self) {
        let pot = self.root.join("po/sustain.pot");
        let init = self.root.join("po/fr.init.po");
        let fr = self.root.join("po/fr.po");
        gettext_tool("msginit")
            .arg("--no-translator")
            .arg("--locale=fr_FR.UTF-8")
            .arg("-i")
            .arg(&pot)
            .arg("-o")
            .arg(&init)
            .status()
            .expect("run msginit");
        gettext_tool("msgen")
            .arg(&init)
            .arg("-o")
            .arg(&fr)
            .status()
            .expect("run msgen");
        std::fs::remove_file(&init).expect("remove msginit scratch");
        self.write("po/LINGUAS", "fr\n");
    }
}

/// A gettext CLI command with a forced C locale (stable, parseable output).
fn gettext_tool(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env("LC_ALL", "C");
    command
}

/// Run a check expected to fail, returning the error text.
fn expect_check_failure(root: &Path) -> String {
    xtask::check::run_in(root).expect_err("expected i18n-check to fail")
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn extract_captures_call_shapes_and_metadata() {
    let fixture = Fixture::standard("extract_capture");
    xtask::extract::run_in(&fixture.root).expect("extract");
    let pot = fixture.pot();

    assert!(pot.contains("msgid \"Songs\""), "{pot}");
    assert!(pot.contains("msgctxt \"column header\""), "{pot}");
    assert!(pot.contains("msgid \"Play\""), "{pot}");
    assert!(pot.contains("msgctxt \"queue\""), "{pot}");
    assert!(pot.contains("msgid \"{count} song imported\""), "{pot}");
    assert!(
        pot.contains("msgid_plural \"{count} songs imported\""),
        "{pot}"
    );
    assert!(pot.contains("#, rust-format"), "{pot}");
    // Desktop and AppStream metadata join the same template.
    assert!(pot.contains("msgid \"Fixture comment\""), "{pot}");
    assert!(pot.contains("msgid \"Fixture summary\""), "{pot}");
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn check_passes_on_clean_fixture_with_catalog() {
    let fixture = Fixture::standard("check_clean");
    xtask::extract::run_in(&fixture.root).expect("extract");
    fixture.add_complete_fr_catalog();
    xtask::check::run_in(&fixture.root).expect("check should pass on a complete catalog");
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn check_flags_positional_placeholder() {
    let fixture = Fixture::standard("check_positional");
    fixture.write(
        "crates/ui_gtk/src/lib.rs",
        "use sustain_i18n::gettext;\npub fn ui() -> String { gettext(\"Imported {} tracks\") }\n",
    );
    xtask::extract::run_in(&fixture.root).expect("extract");
    let message = expect_check_failure(&fixture.root);
    assert!(message.contains("non-named placeholder"), "{message}");
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn check_flags_qualified_call() {
    let fixture = Fixture::standard("check_qualified");
    fixture.write(
        "crates/ui_gtk/src/extra.rs",
        "pub fn x() -> String { sustain_i18n :: gettext (\"Hidden\") }\n",
    );
    xtask::extract::run_in(&fixture.root).expect("extract");
    let message = expect_check_failure(&fixture.root);
    assert!(message.contains("invisible to xgettext"), "{message}");
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn check_flags_unqualified_call_outside_roots() {
    let fixture = Fixture::standard("check_boundary");
    fixture.write(
        "crates/domain/src/leak.rs",
        "pub fn x() -> String { gettext(\"Domain leak\") }\n",
    );
    xtask::extract::run_in(&fixture.root).expect("extract");
    let message = expect_check_failure(&fixture.root);
    assert!(
        message.contains("outside the extraction roots"),
        "{message}"
    );
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn check_flags_header_tampering() {
    let fixture = Fixture::standard("check_header");
    xtask::extract::run_in(&fixture.root).expect("extract");
    let tampered = fixture
        .pot()
        .replace("the same license as the Sustain package", "AUDIT MARKER");
    fixture.write("po/sustain.pot", &tampered);
    let message = expect_check_failure(&fixture.root);
    assert!(message.contains("header differs"), "{message}");
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn check_flags_untranslated_and_orphan_catalogs() {
    let fixture = Fixture::standard("check_catalog_problems");
    xtask::extract::run_in(&fixture.root).expect("extract");
    fixture.add_complete_fr_catalog();
    // Blank one translation: now untranslated.
    let fr = fixture
        .read("po/fr.po")
        .replacen("msgstr \"Songs\"", "msgstr \"\"", 1);
    fixture.write("po/fr.po", &fr);
    let untranslated = expect_check_failure(&fixture.root);
    assert!(untranslated.contains("untranslated"), "{untranslated}");

    // A catalog file not listed in LINGUAS is an orphan.
    fixture.write("po/LINGUAS", "# none listed\n");
    let orphan = expect_check_failure(&fixture.root);
    assert!(orphan.contains("not listed in po/LINGUAS"), "{orphan}");
}

#[test]
#[ignore = "requires GNU gettext >= 0.24; run in the i18n CI job"]
fn compile_builds_artifacts_and_clears_stale_locale() {
    let fixture = Fixture::standard("compile_clean");
    xtask::extract::run_in(&fixture.root).expect("extract");
    fixture.add_complete_fr_catalog();

    // Plant a stale catalog in the artifact tree from a "previous" run.
    let stale = fixture
        .root
        .join("target/i18n/dist/locale/zz/LC_MESSAGES")
        .join("sustain.mo");
    std::fs::create_dir_all(stale.parent().expect("stale parent")).expect("create stale dir");
    std::fs::write(&stale, b"stale").expect("write stale mo");

    xtask::compile::run_in(&fixture.root).expect("compile");

    let dist = fixture.root.join("target/i18n/dist");
    assert!(
        dist.join("locale/fr/LC_MESSAGES/sustain.mo").is_file(),
        "fr catalog compiled"
    );
    assert!(
        dist.join(format!("{APP_ID}.desktop")).is_file(),
        "desktop built"
    );
    assert!(
        dist.join(format!("{APP_ID}.metainfo.xml")).is_file(),
        "metainfo built"
    );
    assert!(
        !stale.exists(),
        "stale zz catalog must be cleared by the swap"
    );
}
