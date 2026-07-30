// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Wavefront OBJ export, for the human gates.
//!
//! Deliverable 3 asks whether step-up at 1/3 yard "FEELS right", judged in a
//! minimal visualisation. That is an `[H]` criterion — no test can pass it, and
//! this spike has no renderer and no business growing one.
//!
//! So instead of a debug camera, this writes the meshed scene and the recorded
//! player trajectory as OBJ files. Any 3D viewer opens them, orbits them, and
//! shows the path a body actually took across chiselled geometry. That is the
//! same information a throwaway orbit camera would have shown, without a
//! throwaway renderer to maintain, and it puts the artefact in a form that
//! outlives the spike.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

use crate::mesher::Mesh;

/// Writes a mesh as an OBJ file, one group per material.
///
/// # Errors
///
/// Any I/O failure writing the file.
pub fn write_mesh_obj(path: &Path, mesh: &Mesh, label: &str) -> io::Result<()> {
    let mut out = String::with_capacity(mesh.quads.len() * 96);
    let _ = writeln!(out, "# Tiamot sub-node spike — {label}");
    let _ = writeln!(
        out,
        "# {} quads. Units are sub-nodes: 3 per block, 1 block = 1 yard.",
        mesh.quads.len()
    );

    let (vertices, _) = mesh.to_buffers();
    for vertex in &vertices {
        let x = vertex.packed & 0x3F;
        let y = (vertex.packed >> 6) & 0x3F;
        let z = (vertex.packed >> 12) & 0x3F;
        // Scale to yards so the viewer shows real-world proportions: a player
        // is 1.8 units tall in this file, which is what makes a 1/3-yard step
        // legible as a step.
        let _ = writeln!(
            out,
            "v {:.4} {:.4} {:.4}",
            x as f32 / 3.0,
            y as f32 / 3.0,
            z as f32 / 3.0
        );
    }

    for quad in 0..mesh.quads.len() {
        let base = quad * 4 + 1;
        let _ = writeln!(out, "f {} {} {} {}", base, base + 1, base + 2, base + 3);
    }

    std::fs::write(path, out)
}

/// Writes a trajectory as an OBJ polyline.
///
/// # Errors
///
/// Any I/O failure writing the file.
pub fn write_path_obj(path: &Path, points: &[[f32; 3]], label: &str) -> io::Result<()> {
    let mut out = String::with_capacity(points.len() * 48);
    let _ = writeln!(out, "# Tiamot sub-node spike — {label}");
    let _ = writeln!(out, "# {} ticks at 20 tps. Units are yards.", points.len());

    for point in points {
        let _ = writeln!(
            out,
            "v {:.4} {:.4} {:.4}",
            point[0] / 3.0,
            point[1] / 3.0,
            point[2] / 3.0
        );
    }

    if points.len() >= 2 {
        let _ = write!(out, "l");
        for index in 1..=points.len() {
            let _ = write!(out, " {index}");
        }
        let _ = writeln!(out);
    }

    std::fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesher::{SubNodeGrid, mesh};
    use crate::scenes::Scene;

    #[test]
    fn mesh_export_has_one_face_per_quad_and_four_vertices_each() {
        let chunk = Scene::Realistic.build(1);
        let meshed = mesh(&SubNodeGrid::from_chunk(&chunk));
        let dir = std::env::temp_dir().join("tiamot-spike-export-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("mesh.obj");

        write_mesh_obj(&path, &meshed, "test").expect("write");
        let text = std::fs::read_to_string(&path).expect("read");

        let vertices = text.lines().filter(|line| line.starts_with("v ")).count();
        let faces = text.lines().filter(|line| line.starts_with("f ")).count();
        assert_eq!(faces, meshed.quads.len());
        assert_eq!(vertices, meshed.quads.len() * 4);
    }

    #[test]
    fn path_export_writes_a_single_polyline() {
        let dir = std::env::temp_dir().join("tiamot-spike-export-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("path.obj");
        let points = vec![[0.0, 1.0, 2.0], [3.0, 4.0, 5.0], [6.0, 7.0, 8.0]];

        write_path_obj(&path, &points, "test").expect("write");
        let text = std::fs::read_to_string(&path).expect("read");

        assert_eq!(text.lines().filter(|l| l.starts_with("v ")).count(), 3);
        assert_eq!(text.lines().filter(|l| l.starts_with("l ")).count(), 1);
    }
}
