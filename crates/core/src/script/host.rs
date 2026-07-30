// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The engine-driven mod lifecycle.
//!
//! Charter rule 9's sequence, in one place:
//!
//! ```text
//! scan → resolve → load each in order → CLOSE the registration window →
//! FREEZE → world load → play
//! ```
//!
//! The ordering is not stylistic. Registration order decides numeric material
//! ids, numeric ids go into every chunk blob (charter rule 8), and the world
//! database reconciles against them on open (Task 03). A lifecycle that loaded
//! mods in a different order would hand the same world different ids and
//! reinterpret every block in it.
//!
//! After [`ModHost::freeze`] the registries are immutable and `register_*` is a
//! hard error from Lua. That is what makes the id table safe to persist.

use std::path::Path;

use crate::chunk::Chunk;
use crate::coords::ChunkPos;
use crate::material::MaterialId;
use crate::modload::{ModManifest, ResolvedSet, resolve, scan_directory};
use crate::script::vm::{ScriptError, ScriptVm, VmLimits};

/// Something went wrong bringing mods up.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// A mod directory could not be scanned or a manifest was invalid.
    ///
    /// Boxed: a `ManifestError` carries a `toml` parse error, which is large
    /// enough that every `Result` on the startup path would pay for it.
    #[error("could not read mods from `{root}`")]
    Scan {
        /// The directory scanned.
        root: String,
        /// Why.
        #[source]
        source: Box<crate::modload::ManifestError>,
    },

    /// Dependencies could not be resolved.
    #[error("could not resolve mod dependencies")]
    Resolve(#[from] Box<crate::modload::ResolveError>),

    /// A mod's entry script could not be read.
    #[error("could not read `{path}`")]
    ReadEntry {
        /// The path.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// The script VM failed.
    #[error(transparent)]
    Script(#[from] ScriptError),
}

/// Which lifecycle phase the host is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Mods are loading; `register_*` is permitted.
    Registration,
    /// Registries are immutable; generation may run.
    Frozen,
}

/// Owns the mod set and the VM running it.
pub struct ModHost<V: ScriptVm> {
    vm: V,
    resolved: ResolvedSet,
    phase: Phase,
    /// Mods that failed to load. They are skipped, not fatal.
    failed: Vec<(String, ScriptError)>,
}

impl<V: ScriptVm> ModHost<V> {
    /// Scans a directory, resolves it, and loads every mod in order.
    ///
    /// A mod whose script **fails to load is disabled, not fatal** (charter
    /// rule 10): the rest of the set still comes up, and the failure is
    /// recorded in [`Self::failed`]. A mod set that fails to *resolve* is fatal,
    /// because there is no correct subset to fall back to.
    ///
    /// # Errors
    ///
    /// [`HostError`] if scanning, resolution, or VM creation fails.
    pub fn load_from(root: &Path, limits: VmLimits) -> Result<Self, HostError> {
        let discovered = scan_directory(root).map_err(|source| HostError::Scan {
            root: root.display().to_string(),
            source: Box::new(source),
        })?;
        let resolved = resolve(&discovered).map_err(Box::new)?;

        let mut vm = V::create(limits)?;
        let mut failed = Vec::new();

        for entry in &resolved.order {
            let path = entry.dir.join(crate::modload::manifest_entry_file());
            let source = std::fs::read_to_string(&path).map_err(|source| HostError::ReadEntry {
                path: path.display().to_string(),
                source,
            })?;

            if let Err(err) = vm.load_mod(&entry.id, &source, &entry.dir) {
                tracing::error!(
                    mod_id = %entry.id,
                    error = %err,
                    "mod failed to load and is disabled; the rest of the set continues"
                );
                vm.mark_faulted(&entry.id);
                failed.push((entry.id.clone(), err));
            }
        }

        Ok(Self {
            vm,
            resolved,
            phase: Phase::Registration,
            failed,
        })
    }

    /// Closes the registration window.
    ///
    /// # Errors
    ///
    /// [`ScriptError`] if the VM cannot be updated.
    pub fn freeze(&mut self) -> Result<(), ScriptError> {
        self.vm.freeze()?;
        self.phase = Phase::Frozen;
        Ok(())
    }

    /// The current lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The resolved mod set, in load order.
    #[must_use]
    pub const fn resolved(&self) -> &ResolvedSet {
        &self.resolved
    }

    /// Mods that failed to load, with the reason.
    #[must_use]
    pub fn failed(&self) -> &[(String, ScriptError)] {
        &self.failed
    }

    /// Mods currently disabled — failed to load, or faulted at runtime.
    #[must_use]
    pub fn disabled(&self) -> Vec<String> {
        self.vm.faulted_mods()
    }

    /// Generates one chunk by running the registered callbacks.
    ///
    /// # Errors
    ///
    /// [`ScriptError`] naming the mod that failed. That mod is disabled; later
    /// chunks generate without it.
    pub fn generate_chunk(
        &mut self,
        world_seed: u64,
        pos: ChunkPos,
        fill: MaterialId,
    ) -> Result<Chunk, ScriptError> {
        self.vm.generate_chunk(world_seed, pos, fill)
    }

    /// The VM, for tests and diagnostics.
    pub fn vm_mut(&mut self) -> &mut V {
        &mut self.vm
    }

    /// The VM.
    #[must_use]
    pub const fn vm(&self) -> &V {
        &self.vm
    }
}

/// Reads a mod's manifest without loading it. Diagnostics and `--check-mods`.
///
/// # Errors
///
/// [`crate::modload::ManifestError`] if the manifest is missing or invalid.
pub fn read_manifest(dir: &Path) -> Result<ModManifest, crate::modload::ManifestError> {
    ModManifest::load(dir)
}
