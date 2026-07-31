// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Dry-run mod validation: `server --check-mods <dir>`.
//!
//! Loads, resolves, and runs the full registration window against a real script
//! VM, then reports what happened — **without touching a world**.
//!
//! # Why this is not just "start the server and see"
//!
//! A mod API you cannot check without booting a world is an API with a
//! usability bug. A modder wants the answer in a second, from a terminal, with
//! no database file left behind; CI wants an exit code. Booting a server to
//! find out means creating a world, binding a socket, and then reading the log
//! carefully enough to notice that one mod quietly disabled itself.
//!
//! # A disabled mod is a failure here, unlike at runtime
//!
//! Charter rule 10 says a mod that fails to load is disabled rather than fatal:
//! a live server with 40 players should not die because one mod has a typo.
//! That is the right call *at runtime* and the wrong one *at check time* —
//! the entire point of this command is to notice the typo, so anything that
//! would be silently disabled is reported and exits non-zero.

use std::path::Path;

use tiamot_core::material::Registry;
use tiamot_core::script::{MluaVm, ModHost, ScriptVm as _, VmLimits};

/// What a dry run found.
#[derive(Debug, Default)]
pub struct CheckReport {
    /// Mods that loaded, in resolved order.
    pub loaded: Vec<String>,
    /// Mods that failed to load, with the reason.
    pub failed: Vec<(String, String)>,
    /// Blocks registered, in id order.
    pub blocks: Vec<String>,
    /// Non-fatal observations worth printing.
    pub warnings: Vec<String>,
}

impl CheckReport {
    /// Whether the mod set is usable.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Runs the dry check.
///
/// # Errors
///
/// A human-readable message if the set could not be scanned or resolved at all.
/// A set that *resolves* but contains failing mods is not an error here — it is
/// a [`CheckReport`] with entries in [`CheckReport::failed`], because reporting
/// every problem beats stopping at the first.
pub fn check(dir: &Path) -> Result<CheckReport, String> {
    if !dir.exists() {
        return Err(format!("no such directory: `{}`", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("`{}` is not a directory", dir.display()));
    }

    let mut host = ModHost::<MluaVm>::load_from(dir, VmLimits::default()).map_err(|err| {
        // The whole chain: "could not resolve mod dependencies" with no cause
        // names nothing a modder can act on.
        let mut message = err.to_string();
        let mut source = std::error::Error::source(&err);
        while let Some(cause) = source {
            message.push_str(&format!("\n  caused by: {cause}"));
            source = cause.source();
        }
        message
    })?;

    let mut report = CheckReport::default();

    for entry in &host.resolved().order {
        report
            .loaded
            .push(format!("{} {}", entry.id, entry.version));
    }
    for (mod_id, err) in host.failed() {
        report.failed.push((mod_id.clone(), err.to_string()));
    }

    // FREEZE, then check that registration is actually closed. A VM that
    // accepted registrations afterwards would let a mod add a block on the
    // first tick, and its numeric id would differ between a fresh world and a
    // reloaded one.
    if let Err(err) = host.freeze() {
        report.failed.push(("<freeze>".to_owned(), err.to_string()));
        return Ok(report);
    }

    // Run a tick. A mod that registers from a callback, or that simply blows
    // up on its first tick, is invisible until something calls it — and both
    // would be silently disabled on a live server, which is the failure mode
    // this command exists to surface. Checking `game/` in CI without this would
    // pass a mod that dies the moment a player joins.
    match host.vm_mut().tick(1) {
        Ok(faults) => {
            for (mod_id, err) in faults {
                report
                    .failed
                    .push((mod_id, format!("failed on its first tick: {err}")));
            }
        }
        Err(err) => report.failed.push(("<tick>".to_owned(), err.to_string())),
    }

    let blocks = host.vm().registered_blocks();
    let mut registry = Registry::new();
    for (name, expected) in &blocks {
        report.blocks.push(name.clone());
        match registry.register(name) {
            Ok(assigned) if assigned == *expected => {}
            Ok(assigned) => report.failed.push((
                name.clone(),
                format!(
                    "the engine would assign id {} but the script VM handed out {} — every \
                     block this mod places would be the wrong material",
                    assigned.0, expected.0
                ),
            )),
            Err(err) => report.failed.push((name.clone(), err.to_string())),
        }
    }

    if blocks.is_empty() {
        report
            .warnings
            .push("no blocks registered; this mod set has no placeable content".to_owned());
    }
    if host.resolved().order.is_empty() {
        report
            .warnings
            .push(format!("no mods found under `{}`", dir.display()));
    }

    Ok(report)
}

/// Prints a report for a human and returns the process exit code.
#[must_use]
pub fn report_and_code(dir: &Path, report: &CheckReport) -> u8 {
    println!("checking mods in `{}`", dir.display());

    if report.loaded.is_empty() {
        println!("  no mods loaded");
    } else {
        println!("  {} mod(s) loaded, in order:", report.loaded.len());
        for entry in &report.loaded {
            println!("    {entry}");
        }
    }

    if !report.blocks.is_empty() {
        println!("  {} block(s) registered:", report.blocks.len());
        for (index, name) in report.blocks.iter().enumerate() {
            // The id is shown because it is what ends up in the world file, and
            // a modder debugging a wrong-material bug needs to see it.
            println!("    {name} (id {})", index + 2);
        }
    }

    for warning in &report.warnings {
        println!("  warning: {warning}");
    }

    if report.failed.is_empty() {
        println!("OK");
        0
    } else {
        // To stderr: a CI log is read by searching for the failure, and a
        // failure on stdout gets lost among the successes.
        eprintln!("  {} mod(s) FAILED:", report.failed.len());
        for (mod_id, reason) in &report.failed {
            eprintln!("    {mod_id}: {reason}");
        }
        eprintln!(
            "FAILED — these mods would be silently disabled on a live server (charter rule 10), \
             which is exactly what this command exists to surface."
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("tiamot-checkmods").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write_mod(root: &Path, id: &str, manifest_extra: &str, source: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).expect("mod dir");
        std::fs::write(
            dir.join("mod.toml"),
            format!(
                "id = \"{id}\"\nname = \"{id}\"\nversion = \"0.1.0\"\n\
                 license = \"GPL-3.0-only\"\n{manifest_extra}"
            ),
        )
        .expect("manifest");
        std::fs::write(dir.join("init.lua"), source).expect("script");
    }

    #[test]
    fn the_repository_reference_mods_check_clean() {
        // `game/` is what CI runs this against from Task 07 onward. If the
        // reference mods stop checking clean, the public mod API has broken.
        let repo_game = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../game");
        let report = check(&repo_game).expect("game/ should resolve");

        assert!(
            report.is_ok(),
            "the reference mods should check clean, got failures: {:?}",
            report.failed
        );
        assert!(
            report.loaded.len() >= 2,
            "expected the reference mods, got {:?}",
            report.loaded
        );
        assert!(
            report.blocks.iter().any(|name| name == "core:white"),
            "core:white should be registered, got {:?}",
            report.blocks
        );
    }

    #[test]
    fn a_missing_dependency_is_an_error_naming_what_is_missing() {
        let dir = scratch("missing-dep");
        write_mod(&dir, "needy", "depends = [\"absent >=1.0\"]", "");

        let err = check(&dir).expect_err("a missing dependency must fail");
        assert!(
            err.contains("absent"),
            "the message must name the missing dependency: {err}"
        );
    }

    #[test]
    fn a_mod_that_registers_after_freeze_is_reported() {
        // Charter rule 9's window, checked without booting a world. This is one
        // of the two cases the acceptance criteria name specifically.
        //
        // The registration only fails when the hook RUNS, so loading alone
        // cannot see it — which is why the check runs a tick.
        let dir = scratch("post-freeze");
        write_mod(
            &dir,
            "late",
            "",
            "game.register_on_tick(function() game.register_block{ id = 'too_late' } end)",
        );

        let report = check(&dir).expect("resolves");

        assert!(
            !report.is_ok(),
            "a post-freeze registration must fail the check"
        );
        assert!(
            report
                .failed
                .iter()
                .any(|(id, reason)| id == "late" && reason.contains("first tick")),
            "the failing mod must be named with a readable reason: {:?}",
            report.failed
        );
        assert!(
            !report.blocks.iter().any(|name| name.contains("too_late")),
            "and the registration must not have taken effect"
        );
    }

    #[test]
    fn a_mod_that_dies_on_its_first_tick_is_reported() {
        // The same mechanism catches an ordinary runtime error, which would
        // otherwise be invisible until a player joined a live server.
        let dir = scratch("tick-error");
        write_mod(
            &dir,
            "explodes",
            "",
            "game.register_on_tick(function() error('boom') end)",
        );

        let report = check(&dir).expect("resolves");
        assert!(!report.is_ok());
        assert!(
            report.failed.iter().any(|(id, _)| id == "explodes"),
            "{:?}",
            report.failed
        );
    }

    #[test]
    fn a_syntax_error_is_reported_rather_than_silently_disabling_the_mod() {
        // At runtime this mod would be disabled and the server would carry on.
        // Here it must be loud: catching the typo is the entire point.
        let dir = scratch("syntax-error");
        write_mod(&dir, "good", "", "game.register_block{ id = 'fine' }");
        write_mod(&dir, "broken", "", "this is not lua ((((");

        let report = check(&dir).expect("resolves");

        assert!(!report.is_ok(), "a broken mod must fail the check");
        assert!(
            report.failed.iter().any(|(id, _)| id == "broken"),
            "the failing mod must be named: {:?}",
            report.failed
        );
        assert!(
            report.blocks.iter().any(|name| name == "good:fine"),
            "the working mod should still have registered"
        );
    }

    #[test]
    fn a_missing_directory_is_a_clear_error_not_a_panic() {
        let err = check(Path::new("/definitely/not/here")).expect_err("must fail");
        assert!(err.contains("no such directory"), "{err}");
    }

    #[test]
    fn an_empty_directory_warns_rather_than_failing() {
        // A server with no mods is legitimate — the engine is mechanisms — so
        // this is a warning, not an error.
        let dir = scratch("empty");
        let report = check(&dir).expect("an empty directory resolves");
        assert!(report.is_ok());
        assert!(
            report.warnings.iter().any(|w| w.contains("no mods")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn the_exit_code_is_zero_only_when_nothing_failed() {
        let clean = CheckReport {
            loaded: vec!["a 0.1.0".to_owned()],
            blocks: vec!["a:x".to_owned()],
            ..CheckReport::default()
        };
        assert_eq!(report_and_code(Path::new("."), &clean), 0);

        let broken = CheckReport {
            failed: vec![("bad".to_owned(), "boom".to_owned())],
            ..CheckReport::default()
        };
        assert_eq!(report_and_code(Path::new("."), &broken), 1);
    }
}
