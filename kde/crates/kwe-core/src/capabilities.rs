// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-1c: the scene capability SUPPORT tables `wallpaper.apply`'s staged
//! inspection gate (`crates/kwe-daemon/src/apply.rs`) classifies a scene's
//! `required` capability ids against.
//!
//! Source of truth is `docs/SCENE_CAPABILITIES.md` (frozen v1) — these two
//! consts are a mechanical projection of that document's status column, not
//! an independent judgment. Every change to the doc's statuses must update
//! these; SR-16's evidence DB is planned to replace both consts with a real
//! database, at which point this module becomes that database's static
//! seed rather than the whole story.

/// The scene renderer is the open-wallpaper-engine backend
/// (`../kwe/SceneRenderer` in the OWE workspace), which implements the full
/// Wallpaper Engine scene family: image/video/text layers, sound layers,
/// effects, PBR lighting, particles, puppets, 3D models, scenescript, and
/// user properties. Every capability id the inspector can currently emit is
/// therefore implemented; this projection replaces the retired Rust
/// renderer's partial table.
///
/// Sorted (deterministic diffs when this list changes) and disjoint from
/// `SCENE_CAPABILITIES_LIMITATION_TOLERATED` — both invariants are
/// unit-tested below.
pub const SCENE_CAPABILITIES_IMPLEMENTED: &[&str] = &[
    "scene.audio-buffers",
    "scene.blend",
    "scene.effects",
    "scene.input.cursor-events",
    "scene.layer.image",
    "scene.layer.sound",
    "scene.layer.text",
    "scene.layer.video",
    "scene.lighting",
    "scene.material",
    "scene.package",
    "scene.particle",
    "scene.property.binding",
    "scene.property.unknown-preservation",
    "scene.render-target",
    "scene.script.lifecycle",
    "scene.script.objects",
    "scene.shader",
    "scene.simulation-pause",
    "scene.texture.animated",
    "scene.texture.compressed",
    "scene.texture.static",
    "scene.texture.texv",
];

/// Capabilities whose absence cannot blank a scene or break layer
/// ordering/dependencies (plan §5.2's optional-enhancement rule). Empty
/// with the OWE backend: the previously tolerated `scene.layer.sound` and
/// `scene.lighting` are now implemented. A scene requiring ONLY tolerated
/// capabilities beyond what is implemented still applies; the apply result
/// and `renderer.status` carry the limitation so it is diagnosable, never
/// silently swallowed.
pub const SCENE_CAPABILITIES_LIMITATION_TOLERATED: &[&str] = &[];

#[cfg(test)]
mod tests {
    use super::*;

    fn is_sorted(list: &[&str]) -> bool {
        list.windows(2).all(|pair| pair[0] < pair[1])
    }

    #[test]
    fn implemented_and_tolerated_are_sorted_and_disjoint() {
        assert!(
            is_sorted(SCENE_CAPABILITIES_IMPLEMENTED),
            "SCENE_CAPABILITIES_IMPLEMENTED must stay sorted for deterministic diffs"
        );
        assert!(
            is_sorted(SCENE_CAPABILITIES_LIMITATION_TOLERATED),
            "SCENE_CAPABILITIES_LIMITATION_TOLERATED must stay sorted for deterministic diffs"
        );
        for capability in SCENE_CAPABILITIES_LIMITATION_TOLERATED {
            assert!(
                !SCENE_CAPABILITIES_IMPLEMENTED.contains(capability),
                "{capability} must not appear in both sets"
            );
        }
    }

    /// SR-0d's real corpus run (docs/SR0.md, "Real-corpus run (conductor,
    /// 2026-08-28, local lab, metadata only)": 92 items, 60 inspected, all
    /// 60 inventoried:ok) recorded this exact `required` capability
    /// histogram: scene.package 60, scene.layer.image 60, scene.effects 48,
    /// scene.particle 33, scene.layer.sound 19, scene.layer.text 14 (the
    /// full corpus record is uncommitted local metadata, per the SR-0d
    /// content policy — the histogram itself is copied into docs/SR0.md).
    /// This gate must stay corpus-neutral: every one of those ids must be
    /// implemented or tolerated, or SR-1c's own conductor decision (a) —
    /// "every one of the 60 local corpus scenes keeps applying" — would
    /// already be false the day this table landed.
    #[test]
    fn every_capability_the_local_corpus_actually_required_is_covered() {
        const CORPUS_REQUIRED_2026_08_28: &[&str] = &[
            "scene.package",
            "scene.layer.image",
            "scene.effects",
            "scene.particle",
            "scene.layer.sound",
            "scene.layer.text",
        ];
        for capability in CORPUS_REQUIRED_2026_08_28 {
            assert!(
                SCENE_CAPABILITIES_IMPLEMENTED.contains(capability)
                    || SCENE_CAPABILITIES_LIMITATION_TOLERATED.contains(capability),
                "{capability} (seen in the 2026-08-28 corpus run) is neither \
                 implemented nor tolerated — the apply gate would now refuse \
                 real local content"
            );
        }
    }
}
