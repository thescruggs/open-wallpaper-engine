// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded offline metadata cache for subscribed Workshop items.
//!
//! Steam's local manifests only carry ids and progress; titles, tags, and
//! preview availability come from the Workshop files themselves. When a
//! library is unmounted (or the daemon restarts while Steam is offline) that
//! metadata would otherwise vanish. This cache snapshots it per subscribed
//! item and, after each scan, fills placeholder `subscribed_missing` entries
//! and synthesizes entries for subscriptions whose library disappeared — so
//! the M6 exit gate's "library unmounted" leg degrades to an informative
//! offline state instead of silent disappearance.
//!
//! Entries are touched every scan they appear in (a live subscription never
//! ages out); ids absent from every scan for 30 days are dropped, so an
//! unsubscribed item fades deterministically.
//!
//! W5 review fix (C2, R8): an unsubscribe the daemon itself carried out
//! (`workshop.unsubscribe` answered ok) removes the id from the cache and
//! records a tombstone, so the next scan — where Steam has dropped the
//! item — does not synthesize it as a vanished `subscribed_missing`
//! subscription. A tombstone ends when the daemon subscribes to the id
//! again, when a scan shows the id subscribed more than
//! `UNSUBSCRIBE_SETTLE_MS` after the unsubscribe (re-subscribed in the
//! Steam client; earlier sightings are Steam's own lag), or after 30 days.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use kwe_core::{Catalog, CatalogItem, Diagnostic, DiagnosticLevel, ProjectKind};
use serde::{Deserialize, Serialize};

use crate::persist::{atomic_write, quarantine_invalid_state};

const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_FILE: &str = "workshop-metadata-v1.json";
const MAX_CACHE_ITEMS: usize = 25_000;
const MAX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
/// Absent-from-scan entries older than this are dropped (30 days).
const STALE_ENTRY_MS: u128 = 30 * 24 * 60 * 60 * 1000;
/// `ScanLimits::default().max_projects` — synthesis must not exceed it.
const MAX_CATALOG_ITEMS: usize = 25_000;
/// A scan that still shows an unsubscribed id as subscribed within this
/// window is Steam catching up, not a re-subscription (10 minutes).
const UNSUBSCRIBE_SETTLE_MS: u128 = 10 * 60 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedItem {
    title: String,
    kind: ProjectKind,
    tags: Vec<String>,
    preview_present: bool,
    metadata_hash: Option<String>,
    workshop_state: String,
    workshop_progress: Option<u8>,
    last_seen_unix_ms: u128,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedCache {
    schema_version: u32,
    items: BTreeMap<String, CachedItem>,
    /// R8: ids the daemon unsubscribed → when (unix ms). Additive: older
    /// files load with none; older daemons ignore the key.
    #[serde(default)]
    unsubscribed: BTreeMap<String, u128>,
}

/// True when the workspace state means "the user subscribed in Steam".
fn is_subscribed_state(state: &str) -> bool {
    matches!(
        state,
        "subscribed_installed" | "downloading" | "subscribed_missing"
    )
}

/// The scanner's synthetic title for missing items has no user value; the
/// cache overlay may replace it (but never a title from the disk scan).
fn placeholder_title(item: &CatalogItem) -> bool {
    item.title == format!("Workshop item {}", item.workshop_id)
}

pub struct WorkshopCache {
    path: PathBuf,
    items: BTreeMap<String, CachedItem>,
    unsubscribed: BTreeMap<String, u128>,
}

impl WorkshopCache {
    /// Loads the cache, failing closed: a corrupt or oversized file is
    /// quarantined to an `.invalid-*` sibling and the cache starts fresh.
    pub fn open(state_dir: &Path) -> Self {
        let path = state_dir.join(CACHE_FILE);
        let (items, unsubscribed) = match fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_CACHE_BYTES => {
                eprintln!("event=workshop.cache_oversize bytes={}", metadata.len());
                quarantine_invalid_state(&path);
                (BTreeMap::new(), BTreeMap::new())
            }
            Ok(_) => match fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<PersistedCache>(&bytes) {
                    Ok(cache)
                        if cache.schema_version == CACHE_SCHEMA_VERSION
                            && cache.items.len() <= MAX_CACHE_ITEMS
                            && cache.unsubscribed.len() <= MAX_CACHE_ITEMS =>
                    {
                        (cache.items, cache.unsubscribed)
                    }
                    Ok(_) => {
                        eprintln!(
                            "event=workshop.cache_invalid detail=unsupported or oversized cache"
                        );
                        quarantine_invalid_state(&path);
                        (BTreeMap::new(), BTreeMap::new())
                    }
                    Err(error) => {
                        eprintln!("event=workshop.cache_invalid detail={error}");
                        quarantine_invalid_state(&path);
                        (BTreeMap::new(), BTreeMap::new())
                    }
                },
                Err(error) => {
                    eprintln!("event=workshop.cache_read_error detail={error}");
                    quarantine_invalid_state(&path);
                    (BTreeMap::new(), BTreeMap::new())
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (BTreeMap::new(), BTreeMap::new())
            }
            Err(error) => {
                eprintln!("event=workshop.cache_read_error detail={error}");
                (BTreeMap::new(), BTreeMap::new())
            }
        };
        Self {
            path,
            items,
            unsubscribed,
        }
    }

    /// R8: the daemon unsubscribed `id` (Steam accepted it). Drops its
    /// cached entry and remembers the unsubscribe, so a later scan without
    /// the id does not synthesize it as a vanished subscription. Returns
    /// true when the cache changed and a `save()` is worthwhile.
    pub fn forget_unsubscribed(&mut self, id: &str, now_unix_ms: u128) -> bool {
        self.items.remove(id);
        if self.unsubscribed.len() >= MAX_CACHE_ITEMS && !self.unsubscribed.contains_key(id) {
            // Bounded: forget the oldest tombstone first.
            if let Some(oldest) = self
                .unsubscribed
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(id, _)| id.clone())
            {
                self.unsubscribed.remove(&oldest);
            }
        }
        self.unsubscribed.insert(id.to_string(), now_unix_ms);
        true
    }

    /// R8: the daemon subscribed to `id` again: it is a subscription like
    /// any other from now on. Returns whether the cache changed.
    pub fn note_subscribed(&mut self, id: &str) -> bool {
        self.unsubscribed.remove(id).is_some()
    }

    /// Fills placeholder metadata on scanned items, snapshots fresh metadata
    /// from subscribed items, synthesizes vanished subscriptions, and drops
    /// aged-out entries. `now_unix_ms` is the caller's wall-clock epoch.
    /// Returns true when the cache changed and a `save()` is worthwhile.
    pub fn merge_and_update(&mut self, catalog: &mut Catalog, now_unix_ms: u128) -> bool {
        let mut updates: Vec<(String, CachedItem)> = Vec::new();
        for item in catalog.items.iter_mut() {
            let cached = self.items.get(&item.workshop_id).cloned();
            if item.workshop_state == "subscribed_missing" {
                // No files to read: restore whatever the cache remembers and
                // keep the entry alive (the subscription itself is current).
                if let Some(cached) = &cached {
                    overlay(item, cached);
                }
                updates.push((
                    item.workshop_id.clone(),
                    CachedItem {
                        title: cached
                            .as_ref()
                            .map_or_else(|| item.title.clone(), |cached| cached.title.clone()),
                        kind: if item.kind == ProjectKind::Invalid {
                            cached.as_ref().map_or(item.kind, |cached| cached.kind)
                        } else {
                            item.kind
                        },
                        tags: cached
                            .as_ref()
                            .map_or_else(|| item.tags.clone(), |cached| cached.tags.clone()),
                        preview_present: cached
                            .as_ref()
                            .is_some_and(|cached| cached.preview_present),
                        metadata_hash: cached
                            .as_ref()
                            .and_then(|cached| cached.metadata_hash.clone()),
                        workshop_state: item.workshop_state.clone(),
                        workshop_progress: item.workshop_progress,
                        last_seen_unix_ms: now_unix_ms,
                    },
                ));
            } else if is_subscribed_state(&item.workshop_state) {
                // Files are on disk, but a re-download can leave placeholders
                // where real metadata used to be. Never overwrite cached real
                // metadata with placeholder or Invalid values.
                updates.push((
                    item.workshop_id.clone(),
                    CachedItem {
                        title: if placeholder_title(item) {
                            cached
                                .as_ref()
                                .map_or_else(|| item.title.clone(), |cached| cached.title.clone())
                        } else {
                            item.title.clone()
                        },
                        kind: if item.kind == ProjectKind::Invalid {
                            cached.as_ref().map_or(item.kind, |cached| cached.kind)
                        } else {
                            item.kind
                        },
                        tags: if item.tags.is_empty() {
                            cached
                                .as_ref()
                                .map_or_else(Vec::new, |cached| cached.tags.clone())
                        } else {
                            item.tags.clone()
                        },
                        preview_present: item.preview_file.is_some()
                            || cached.as_ref().is_some_and(|cached| cached.preview_present),
                        metadata_hash: item.metadata_hash.clone().or_else(|| {
                            cached
                                .as_ref()
                                .and_then(|cached| cached.metadata_hash.clone())
                        }),
                        workshop_state: item.workshop_state.clone(),
                        workshop_progress: item.workshop_progress,
                        last_seen_unix_ms: now_unix_ms,
                    },
                ));
            }
        }
        let mut changed = false;
        // R8: tombstones end when they age out, or when a scan shows the id
        // subscribed after the settle window (re-subscribed in Steam).
        let resubscribed: Vec<String> = self
            .unsubscribed
            .iter()
            .filter(|(id, at)| {
                at.saturating_add(STALE_ENTRY_MS) < now_unix_ms
                    || (at.saturating_add(UNSUBSCRIBE_SETTLE_MS) < now_unix_ms
                        && catalog.items.iter().any(|item| {
                            &item.workshop_id == *id && is_subscribed_state(&item.workshop_state)
                        }))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in resubscribed {
            self.unsubscribed.remove(&id);
            changed = true;
        }
        for (id, entry) in updates {
            // Review fixes D (NIT): a Steam-lag scan of a still-tombstoned
            // id must not re-cache it — the fresh `last_seen` would outlive
            // the tombstone and the id would then be synthesized (and
            // touched) as `subscribed_missing` forever.
            if self.unsubscribed.contains_key(&id) {
                continue;
            }
            let dominated = self.items.get(&id).is_some_and(|existing| {
                existing.title == entry.title
                    && existing.kind == entry.kind
                    && existing.tags == entry.tags
                    && existing.preview_present == entry.preview_present
                    && existing.metadata_hash == entry.metadata_hash
                    && existing.workshop_state == entry.workshop_state
                    && existing.workshop_progress == entry.workshop_progress
            });
            if !dominated {
                changed = true;
            }
            self.items.insert(id, entry);
        }

        // Synthesize entries for subscriptions whose library vanished; drop
        // entries that have not appeared in any scan for too long. Aging and
        // synthesis are separate passes so the synthesis cap never skips
        // aging for later entries.
        let present_ids: std::collections::BTreeSet<String> = catalog
            .items
            .iter()
            .map(|item| item.workshop_id.clone())
            .collect();
        let mut to_synthesize = Vec::new();
        let mut aged_out = Vec::new();
        for (id, entry) in &self.items {
            if present_ids.contains(id) || self.unsubscribed.contains_key(id) {
                continue;
            }
            if entry.last_seen_unix_ms.saturating_add(STALE_ENTRY_MS) < now_unix_ms {
                aged_out.push(id.clone());
                changed = true;
                continue;
            }
            if catalog.items.len() < MAX_CATALOG_ITEMS {
                to_synthesize.push((id.clone(), entry.clone()));
            }
        }
        for id in aged_out {
            self.items.remove(&id);
        }
        for (id, entry) in to_synthesize {
            catalog.items.push(synthesized_item(&id, &entry));
            // Mirror `CatalogStats::from_items` semantics: missing items are
            // never counted as subscribed (that bucket counts installed
            // subscriptions only); the kind counter follows the cached kind.
            catalog.stats.total = catalog.stats.total.saturating_add(1);
            catalog.stats.missing = catalog.stats.missing.saturating_add(1);
            match entry.kind {
                ProjectKind::Scene => catalog.stats.scene = catalog.stats.scene.saturating_add(1),
                ProjectKind::Video => catalog.stats.video = catalog.stats.video.saturating_add(1),
                ProjectKind::Web => catalog.stats.web = catalog.stats.web.saturating_add(1),
                ProjectKind::Unknown => {
                    catalog.stats.unknown = catalog.stats.unknown.saturating_add(1)
                }
                ProjectKind::Invalid => {
                    catalog.stats.invalid = catalog.stats.invalid.saturating_add(1)
                }
            }
            // A synthesized item is still a live subscription: touching it
            // means only ids absent from every scan ever age out.
            if let Some(updated) = self.items.get_mut(&id) {
                updated.last_seen_unix_ms = now_unix_ms;
            }
            changed = true;
        }
        changed
    }

    /// Persists the cache atomically; evicts the oldest entries when the
    /// bounded file size would be exceeded.
    pub fn save(&mut self) {
        // Evict in estimated batches instead of one entry per encode: each
        // pass encodes once, evicts enough oldest entries to cover the
        // overshoot, then re-encodes to confirm. Re-encoding once per eviction
        // was O(N²) when many entries had to go.
        let mut bytes = self.encode_state();
        let mut evicted = 0;
        while (bytes.len() as u64) > MAX_CACHE_BYTES && !self.items.is_empty() {
            let overshoot = bytes.len() as u64 - MAX_CACHE_BYTES;
            let per_entry = ((bytes.len() / self.items.len().max(1)) as u64).max(1);
            let mut to_evict = (overshoot / per_entry + 1).min(self.items.len() as u64);
            while to_evict > 0 {
                let Some(oldest) = self
                    .items
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_seen_unix_ms)
                    .map(|(id, _)| id.clone())
                else {
                    break;
                };
                self.items.remove(&oldest);
                evicted += 1;
                to_evict -= 1;
            }
            bytes = self.encode_state();
        }
        if evicted > 0 {
            eprintln!("event=workshop.cache_evict count={evicted}");
        }
        if bytes.len() as u64 > MAX_CACHE_BYTES {
            eprintln!("event=workshop.cache_oversize bytes={}", bytes.len());
            return;
        }
        if let Err(error) = atomic_write(&self.path, &bytes) {
            eprintln!("event=workshop.cache_save_error detail={error}");
        }
    }

    fn encode_state(&self) -> Vec<u8> {
        serde_json::to_vec_pretty(&PersistedCache {
            schema_version: CACHE_SCHEMA_VERSION,
            items: self.items.clone(),
            unsubscribed: self.unsubscribed.clone(),
        })
        .unwrap_or_default()
    }
}

fn overlay(item: &mut CatalogItem, cached: &CachedItem) {
    let mut changed = false;
    if placeholder_title(item) && !cached.title.is_empty() {
        item.title = cached.title.clone();
        changed = true;
    }
    if item.kind == ProjectKind::Invalid && cached.kind != ProjectKind::Invalid {
        item.kind = cached.kind;
        changed = true;
    }
    if item.tags.is_empty() && !cached.tags.is_empty() {
        item.tags = cached.tags.clone();
        changed = true;
    }
    if item.metadata_hash.is_none() && cached.metadata_hash.is_some() {
        item.metadata_hash = cached.metadata_hash.clone();
        changed = true;
    }
    if changed {
        item.diagnostics.push(Diagnostic {
            code: "workshop.offline_metadata".into(),
            level: DiagnosticLevel::Info,
            message: "Metadata restored from the offline cache; the Steam library is unavailable."
                .into(),
        });
    }
}

fn synthesized_item(id: &str, entry: &CachedItem) -> CatalogItem {
    let mut item = CatalogItem {
        workshop_id: id.to_string(),
        title: if entry.title.is_empty() {
            format!("Workshop item {id}")
        } else {
            entry.title.clone()
        },
        kind: entry.kind,
        compatibility: kwe_core::Compatibility::RendererDependent,
        compatibility_detail: "Subscribed in Steam, but the local Workshop files are unavailable"
            .into(),
        content_root: PathBuf::new(),
        project_file: PathBuf::new(),
        entry_file: None,
        preview_file: None,
        metadata_hash: entry.metadata_hash.clone(),
        tags: entry.tags.clone(),
        requested_permissions: Vec::new(),
        workshop_state: "subscribed_missing".into(),
        workshop_progress: entry.workshop_progress,
        workshop_update_available: false,
        diagnostics: Vec::new(),
    };
    item.diagnostics.push(Diagnostic {
        code: "workshop.offline_metadata".into(),
        level: DiagnosticLevel::Info,
        message: "This subscription's Steam library is currently unavailable; metadata is from the offline cache."
            .into(),
    });
    item
}

/// Catalog fixtures for tests outside this module (R8 service test).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn catalog(items: Vec<CatalogItem>) -> Catalog {
        Catalog {
            schema_version: 1,
            generated_unix_ms: 0,
            libraries: Vec::new(),
            items,
            diagnostics: Vec::new(),
            stats: kwe_core::CatalogStats::default(),
        }
    }

    /// A scanned Workshop item with real metadata in `state`.
    pub(crate) fn item(id: &str, state: &str) -> CatalogItem {
        CatalogItem {
            workshop_id: id.into(),
            title: format!("Title {id}"),
            kind: ProjectKind::Scene,
            compatibility: kwe_core::Compatibility::RendererDependent,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: None,
            metadata_hash: Some("abc".into()),
            tags: vec!["nature".into()],
            requested_permissions: Vec::new(),
            workshop_state: state.into(),
            workshop_progress: None,
            workshop_update_available: false,
            diagnostics: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }

    fn state_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kwe-workshop-cache-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    fn entry(id: &str, title: &str, last_seen: u128) -> (String, CachedItem) {
        (
            id.to_string(),
            CachedItem {
                title: title.into(),
                kind: ProjectKind::Scene,
                tags: vec!["nature".into()],
                preview_present: true,
                metadata_hash: Some("abc".into()),
                workshop_state: "subscribed_installed".into(),
                workshop_progress: None,
                last_seen_unix_ms: last_seen,
            },
        )
    }

    fn empty_catalog() -> Catalog {
        Catalog {
            schema_version: 1,
            generated_unix_ms: 0,
            libraries: Vec::new(),
            items: Vec::new(),
            diagnostics: Vec::new(),
            stats: kwe_core::CatalogStats::default(),
        }
    }

    fn missing_item(id: &str) -> CatalogItem {
        CatalogItem {
            workshop_id: id.into(),
            title: format!("Workshop item {id}"),
            kind: ProjectKind::Invalid,
            compatibility: kwe_core::Compatibility::Invalid,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: None,
            metadata_hash: None,
            tags: Vec::new(),
            requested_permissions: Vec::new(),
            workshop_state: "subscribed_missing".into(),
            workshop_progress: None,
            workshop_update_available: false,
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn fills_placeholder_metadata_on_missing_items() {
        let dir = state_dir("fill");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let seen_before = now_ms();
        let (id, cached) = entry("100", "Real Title", seen_before);
        cache.items.insert(id.clone(), cached);
        let mut catalog = empty_catalog();
        catalog.items.push(missing_item(&id));
        // A later scan time, fixed so the assertion below cannot race the
        // clock (it used to compare against a second now_ms() reading and
        // failed whenever a millisecond boundary fell in between).
        let scan_time = seen_before + 1_000;
        cache.merge_and_update(&mut catalog, scan_time);
        let item = &catalog.items[0];
        assert_eq!(item.title, "Real Title");
        assert_eq!(item.kind, ProjectKind::Scene);
        assert_eq!(item.tags, vec!["nature"]);
        assert!(
            item.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "workshop.offline_metadata")
        );
        // Live subscription entries stay alive even while missing.
        let touched = cache.items[&id].last_seen_unix_ms;
        assert_eq!(touched, scan_time, "entries seen in a scan must be touched");
    }

    #[test]
    fn synthesizes_vanished_subscriptions_and_ages_out_unsubscribed() {
        let dir = state_dir("synthesize");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let now = now_ms();
        let (live_id, live) = entry("100", "Live", now);
        let (stale_id, stale) = entry("200", "Stale", now - STALE_ENTRY_MS - 1);
        cache.items.insert(live_id.clone(), live);
        cache.items.insert(stale_id.clone(), stale);
        let mut catalog = empty_catalog();
        cache.merge_and_update(&mut catalog, now);
        assert_eq!(
            catalog.items.len(),
            1,
            "only the fresh entry is synthesized"
        );
        let item = &catalog.items[0];
        assert_eq!(item.workshop_id, live_id);
        assert_eq!(item.title, "Live");
        assert_eq!(item.workshop_state, "subscribed_missing");
        assert!(cache.items.contains_key(&live_id));
        assert!(
            !cache.items.contains_key(&stale_id),
            "aged-out entries are dropped"
        );
    }

    #[test]
    fn snapshots_fresh_metadata_from_installed_items() {
        let dir = state_dir("snapshot");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let mut catalog = empty_catalog();
        catalog.items.push(CatalogItem {
            workshop_id: "7".into(),
            title: "Fresh".into(),
            kind: ProjectKind::Video,
            compatibility: kwe_core::Compatibility::RendererDependent,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: Some(PathBuf::from("/preview.jpg")),
            metadata_hash: Some("hash".into()),
            tags: vec!["calm".into()],
            requested_permissions: Vec::new(),
            workshop_state: "subscribed_installed".into(),
            workshop_progress: None,
            workshop_update_available: false,
            diagnostics: Vec::new(),
        });
        cache.merge_and_update(&mut catalog, now_ms());
        let cached = &cache.items["7"];
        assert_eq!(cached.title, "Fresh");
        assert!(cached.preview_present);
        assert_eq!(cached.metadata_hash.as_deref(), Some("hash"));
    }

    #[test]
    fn local_only_items_are_never_cached() {
        let dir = state_dir("local-only");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let mut catalog = empty_catalog();
        catalog.items.push(CatalogItem {
            workshop_id: "9".into(),
            title: "Local".into(),
            kind: ProjectKind::Scene,
            compatibility: kwe_core::Compatibility::RendererDependent,
            compatibility_detail: String::new(),
            content_root: PathBuf::new(),
            project_file: PathBuf::new(),
            entry_file: None,
            preview_file: None,
            metadata_hash: None,
            tags: Vec::new(),
            requested_permissions: Vec::new(),
            workshop_state: "local".into(),
            workshop_progress: None,
            workshop_update_available: false,
            diagnostics: Vec::new(),
        });
        cache.merge_and_update(&mut catalog, now_ms());
        assert!(cache.items.is_empty());
    }

    #[test]
    fn corrupt_cache_is_quarantined_and_starts_fresh() {
        let dir = state_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(CACHE_FILE), b"garbage").unwrap();
        let cache = WorkshopCache::open(&dir);
        assert!(cache.items.is_empty());
        let quarantined = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".invalid-"));
        assert!(quarantined, "corrupt cache must be quarantined");
    }

    #[test]
    fn merge_reports_unchanged_when_only_last_seen_moves() {
        let dir = state_dir("changed-detect");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let mut catalog = empty_catalog();
        let mut item = missing_item("abc");
        item.title = "Stable Title".into();
        item.workshop_state = "subscribed_installed".into();
        item.kind = ProjectKind::Scene;
        item.metadata_hash = Some("hash-1".into());
        catalog.items.push(item);
        assert!(
            cache.merge_and_update(&mut catalog, now_ms()),
            "first sighting of an entry is a change"
        );
        assert!(
            !cache.merge_and_update(&mut catalog, now_ms() + 1000),
            "unchanged metadata must not report a change"
        );
        catalog.items[0].metadata_hash = Some("hash-2".into());
        assert!(
            cache.merge_and_update(&mut catalog, now_ms() + 2000),
            "a changed metadata hash is a substantive change"
        );
        catalog.items[0].title = "New Title".into();
        assert!(
            cache.merge_and_update(&mut catalog, now_ms() + 3000),
            "a changed title is a substantive change"
        );
    }

    #[test]
    fn save_evicts_oldest_entries_to_stay_within_bound() {
        let dir = state_dir("save-evict");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        // Each entry carries a ~64 KiB tag; 300 of them clearly exceed the
        // 16 MiB file bound, so save() must evict the oldest-first.
        let big_tag: String = "x".repeat(64 * 1024);
        let now = now_ms();
        for index in 0..300u128 {
            let id = format!("id-{index:03}");
            let (id, mut cached) = entry(&id, &format!("Title {index}"), now + index);
            cached.tags = vec![big_tag.clone()];
            cache.items.insert(id, cached);
        }
        cache.save();
        assert!(
            cache.items.contains_key("id-299"),
            "newest entry must survive eviction"
        );
        assert!(
            !cache.items.contains_key("id-000"),
            "oldest entry must be evicted first"
        );
        let metadata = fs::metadata(&cache.path).unwrap();
        assert!(
            metadata.len() <= MAX_CACHE_BYTES,
            "persisted cache must fit the bound, got {} bytes",
            metadata.len()
        );
        let reloaded = WorkshopCache::open(&dir);
        assert_eq!(reloaded.items.len(), cache.items.len());
    }

    /// R8: a tombstone suppresses synthesis; a scan that still shows the id
    /// subscribed inside the settle window (Steam lag) keeps it, one after
    /// the window (re-subscribed in Steam) ends it, and it ages out.
    #[test]
    fn unsubscribe_tombstone_settles_and_ages_out() {
        use super::test_support::{catalog, item};
        let dir = state_dir("tombstone");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let t0 = now_ms();
        cache.merge_and_update(&mut catalog(vec![item("7", "subscribed_installed")]), t0);
        assert!(cache.forget_unsubscribed("7", t0));
        // Steam lag: still listed a minute later; then gone.
        cache.merge_and_update(
            &mut catalog(vec![item("7", "subscribed_installed")]),
            t0 + 60_000,
        );
        let mut scan = catalog(Vec::new());
        cache.merge_and_update(&mut scan, t0 + 120_000);
        assert!(scan.items.is_empty(), "not resurrected after the lag");
        // Seen subscribed after the settle window: a re-subscription.
        let later = t0 + UNSUBSCRIBE_SETTLE_MS + 1;
        cache.merge_and_update(&mut catalog(vec![item("7", "subscribed_installed")]), later);
        let mut scan = catalog(Vec::new());
        cache.merge_and_update(&mut scan, later + 1);
        assert_eq!(scan.items.len(), 1, "re-subscribed in Steam: cached again");
        // Aging: a tombstone older than 30 days is dropped.
        assert!(cache.forget_unsubscribed("8", t0));
        cache.merge_and_update(&mut catalog(Vec::new()), t0 + STALE_ENTRY_MS + 1);
        assert!(!cache.unsubscribed.contains_key("8"));
    }

    /// Review fixes D (NIT): a Steam-lag scan inside the tombstone's life
    /// does not re-cache the id, so after the tombstone ages out (30 days)
    /// the id is never synthesized as `subscribed_missing` — and stays gone.
    #[test]
    fn steam_lag_scan_never_outlives_the_tombstone() {
        use super::test_support::{catalog, item};
        let dir = state_dir("tombstone-lag");
        fs::create_dir_all(&dir).unwrap();
        let mut cache = WorkshopCache::open(&dir);
        let t0 = now_ms();
        cache.merge_and_update(&mut catalog(vec![item("9", "subscribed_installed")]), t0);
        assert!(cache.forget_unsubscribed("9", t0));
        // Steam lag, late in the settle window.
        cache.merge_and_update(
            &mut catalog(vec![item("9", "subscribed_installed")]),
            t0 + UNSUBSCRIBE_SETTLE_MS - 1,
        );
        assert!(
            !cache.items.contains_key("9"),
            "not re-cached while tombstoned"
        );
        // Just after the tombstone's 30 days: no synthesis, now or later.
        for at in [t0 + STALE_ENTRY_MS + 1, t0 + STALE_ENTRY_MS + 60_000] {
            let mut scan = catalog(Vec::new());
            cache.merge_and_update(&mut scan, at);
            assert!(
                scan.items.is_empty(),
                "synthesized at {at}: {:?}",
                scan.items
            );
        }
        assert!(cache.items.is_empty() && cache.unsubscribed.is_empty());
    }
}
