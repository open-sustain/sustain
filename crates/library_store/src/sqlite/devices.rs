// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for device sync: per-device config,
//! the ticked-playlist selection, and the on-device manifest.

use super::*;
use sustain_domain::{
    DeviceKind, DeviceLayout, DeviceRelativePath, FilesPerFolderCap, PlaylistItem, SyncDevice,
    SyncDeviceId, SyncManifestEntry,
};

pub(super) fn save_sync_device(connection: &Connection, device: &SyncDevice) -> StoreResult<()> {
    connection
        .execute(
            r#"
                INSERT INTO sync_devices
                    (id, label, kind, layout, sub_path, files_per_folder_cap, volume_id)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(id) DO UPDATE SET
                    label = excluded.label,
                    kind = excluded.kind,
                    layout = excluded.layout,
                    sub_path = excluded.sub_path,
                    files_per_folder_cap = excluded.files_per_folder_cap,
                    volume_id = excluded.volume_id
                "#,
            params![
                device.id.as_str(),
                device.label,
                device.kind.as_db(),
                device.layout.as_db(),
                device.sub_path.as_str(),
                device.files_per_folder_cap.as_db(),
                device.volume_id,
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn sync_device(
    connection: &Connection,
    id: &SyncDeviceId,
) -> StoreResult<Option<SyncDevice>> {
    let mut statement = connection
        .prepare(
            "SELECT id, label, kind, layout, sub_path, files_per_folder_cap, volume_id \
             FROM sync_devices WHERE id = ?1",
        )
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![id.as_str()])
        .map_err(StoreError::from)?;
    let Some(row) = rows.next().map_err(StoreError::from)? else {
        return Ok(None);
    };
    Ok(Some(device_from_row(row)?))
}

pub(super) fn sync_devices(connection: &Connection) -> StoreResult<Vec<SyncDevice>> {
    let mut statement = connection
        .prepare(
            "SELECT id, label, kind, layout, sub_path, files_per_folder_cap, volume_id \
             FROM sync_devices ORDER BY label, id",
        )
        .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut devices = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        devices.push(device_from_row(row)?);
    }
    Ok(devices)
}

pub(super) fn delete_sync_device(connection: &Connection, id: &SyncDeviceId) -> StoreResult<()> {
    // The selection and manifest rows cascade via their foreign keys.
    connection
        .execute(
            "DELETE FROM sync_devices WHERE id = ?1",
            params![id.as_str()],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn save_device_selection(
    connection: &mut Connection,
    id: &SyncDeviceId,
    selection: &[PlaylistItem],
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    transaction
        .execute(
            "DELETE FROM sync_device_playlists WHERE device_id = ?1",
            params![id.as_str()],
        )
        .map_err(StoreError::from)?;
    for (position, item) in selection.iter().enumerate() {
        let Some((kind, item_id)) = selection_columns(*item) else {
            continue; // folders are not selectable for sync
        };
        transaction
            .execute(
                "INSERT OR REPLACE INTO sync_device_playlists \
                 (device_id, item_kind, item_id, position) VALUES (?1, ?2, ?3, ?4)",
                params![id.as_str(), kind, item_id, position as i64],
            )
            .map_err(StoreError::from)?;
    }
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn device_selection(
    connection: &Connection,
    id: &SyncDeviceId,
) -> StoreResult<Vec<PlaylistItem>> {
    let mut statement = connection
        .prepare(
            "SELECT item_kind, item_id FROM sync_device_playlists \
             WHERE device_id = ?1 ORDER BY position",
        )
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![id.as_str()])
        .map_err(StoreError::from)?;
    let mut selection = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let kind: i64 = row.get(0).map_err(StoreError::from)?;
        let item_id: i64 = row.get(1).map_err(StoreError::from)?;
        selection.push(selection_from_columns(kind, item_id)?);
    }
    Ok(selection)
}

pub(super) fn save_device_manifest(
    connection: &mut Connection,
    id: &SyncDeviceId,
    entries: &[SyncManifestEntry],
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    transaction
        .execute(
            "DELETE FROM sync_manifest WHERE device_id = ?1",
            params![id.as_str()],
        )
        .map_err(StoreError::from)?;
    for entry in entries {
        // Plain INSERT, not INSERT OR REPLACE: the device's rows were just
        // cleared, so a primary-key conflict here can only mean two entries
        // share one on-device path — a planner bug the sync engine now
        // rejects before it gets here. Failing loudly beats silently
        // collapsing the overwrite evidence (#99).
        transaction
            .execute(
                "INSERT INTO sync_manifest \
                 (device_id, track_id, on_device_path, fingerprint) VALUES (?1, ?2, ?3, ?4)",
                params![
                    id.as_str(),
                    entry.track_id.get(),
                    entry.on_device_path.as_str(),
                    entry.fingerprint,
                ],
            )
            .map_err(StoreError::from)?;
    }
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn device_manifest(
    connection: &Connection,
    id: &SyncDeviceId,
) -> StoreResult<Vec<SyncManifestEntry>> {
    let mut statement = connection
        .prepare(
            "SELECT track_id, on_device_path, fingerprint FROM sync_manifest \
             WHERE device_id = ?1 ORDER BY on_device_path",
        )
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![id.as_str()])
        .map_err(StoreError::from)?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let raw_id: i64 = row.get(0).map_err(StoreError::from)?;
        let track_id = TrackId::new(raw_id).ok_or(StoreError::InvalidStoredId(raw_id))?;
        let raw_on_device_path: String = row.get(1).map_err(StoreError::from)?;
        let on_device_path =
            DeviceRelativePath::new(raw_on_device_path.clone()).ok_or_else(|| {
                StoreError::InvalidStoredEnum(format!("device manifest path: {raw_on_device_path}"))
            })?;
        let fingerprint = row.get(2).map_err(StoreError::from)?;
        entries.push(SyncManifestEntry {
            track_id,
            on_device_path,
            fingerprint,
        });
    }
    Ok(entries)
}

fn device_from_row(row: &rusqlite::Row<'_>) -> StoreResult<SyncDevice> {
    let id_text: String = row.get(0).map_err(StoreError::from)?;
    let id = SyncDeviceId::new(id_text.clone()).ok_or(StoreError::InvalidStoredEnum(format!(
        "device id: {id_text}"
    )))?;
    let label = row.get(1).map_err(StoreError::from)?;
    let kind = DeviceKind::from_db(row.get(2).map_err(StoreError::from)?)
        .ok_or_else(|| StoreError::InvalidStoredEnum("device kind".to_owned()))?;
    let layout = DeviceLayout::from_db(row.get(3).map_err(StoreError::from)?)
        .ok_or_else(|| StoreError::InvalidStoredEnum("device layout".to_owned()))?;
    let raw_sub_path: String = row.get(4).map_err(StoreError::from)?;
    let sub_path = DeviceRelativePath::new(raw_sub_path.clone())
        .ok_or_else(|| StoreError::InvalidStoredEnum(format!("device sub-path: {raw_sub_path}")))?;
    let files_per_folder_cap = FilesPerFolderCap::from_db(row.get(5).map_err(StoreError::from)?)
        .ok_or_else(|| StoreError::InvalidStoredEnum("files-per-folder cap".to_owned()))?;
    let volume_id = optional_string(row, 6)?;
    Ok(SyncDevice {
        id,
        label,
        kind,
        layout,
        sub_path,
        files_per_folder_cap,
        volume_id,
    })
}

fn selection_columns(item: PlaylistItem) -> Option<(i64, i64)> {
    match item {
        PlaylistItem::Playlist(id) => Some((0, id.get())),
        PlaylistItem::SmartPlaylist(id) => Some((1, id.get())),
        PlaylistItem::Folder(_) => None,
    }
}

fn selection_from_columns(kind: i64, item_id: i64) -> StoreResult<PlaylistItem> {
    match kind {
        0 => PlaylistId::new(item_id)
            .map(PlaylistItem::Playlist)
            .ok_or(StoreError::InvalidStoredId(item_id)),
        1 => SmartPlaylistId::new(item_id)
            .map(PlaylistItem::SmartPlaylist)
            .ok_or(StoreError::InvalidStoredId(item_id)),
        other => Err(StoreError::InvalidStoredEnum(format!(
            "selection item kind: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use crate::{LibraryStore, SqliteLibraryStore, StoreError};
    use sustain_domain::{
        DeviceKind, DeviceLayout, DeviceRelativePath, FilesPerFolderCap, PlaylistId, PlaylistItem,
        SmartPlaylistId, SyncDevice, SyncDeviceId, SyncManifestEntry, TrackId,
    };

    fn device_path(path: &str) -> DeviceRelativePath {
        DeviceRelativePath::new(path).expect("safe device-relative path")
    }

    fn device(id: &str) -> SyncDevice {
        SyncDevice {
            id: SyncDeviceId::new(id).expect("id"),
            label: "My USB".into(),
            kind: DeviceKind::UsbDrive,
            layout: DeviceLayout::Pioneer,
            sub_path: DeviceRelativePath::root(),
            files_per_folder_cap: FilesPerFolderCap::N128,
            volume_id: Some("VOL-1".into()),
        }
    }

    #[test]
    fn device_round_trips() {
        let store = SqliteLibraryStore::open_in_memory().expect("store");
        let d = device("dev-a");
        store.save_sync_device(&d).expect("save");
        let loaded = store.sync_device(&d.id).expect("load").expect("present");
        assert_eq!(loaded, d);
        assert_eq!(store.sync_devices().expect("list"), vec![d.clone()]);

        // Upsert updates in place.
        let mut updated = d.clone();
        updated.label = "Renamed".into();
        updated.layout = DeviceLayout::M3u;
        store.save_sync_device(&updated).expect("upsert");
        assert_eq!(store.sync_device(&d.id).expect("load"), Some(updated));
    }

    #[test]
    fn selection_round_trips_in_order() {
        let store = SqliteLibraryStore::open_in_memory().expect("store");
        let d = device("dev-b");
        store.save_sync_device(&d).expect("save");
        let selection = vec![
            PlaylistItem::SmartPlaylist(SmartPlaylistId::new(5).expect("id")),
            PlaylistItem::Playlist(PlaylistId::new(2).expect("id")),
        ];
        store
            .save_device_selection(&d.id, &selection)
            .expect("save sel");
        assert_eq!(store.device_selection(&d.id).expect("load sel"), selection);

        // Replacement semantics.
        let next = vec![PlaylistItem::Playlist(PlaylistId::new(9).expect("id"))];
        store
            .save_device_selection(&d.id, &next)
            .expect("replace sel");
        assert_eq!(store.device_selection(&d.id).expect("load"), next);
    }

    #[test]
    fn manifest_round_trips() {
        let store = SqliteLibraryStore::open_in_memory().expect("store");
        let d = device("dev-c");
        store.save_sync_device(&d).expect("save");
        let entries = vec![
            SyncManifestEntry {
                track_id: TrackId::new(1).expect("id"),
                on_device_path: device_path("Contents/A/B/1.mp3"),
                fingerprint: "fp1".into(),
            },
            SyncManifestEntry {
                track_id: TrackId::new(2).expect("id"),
                on_device_path: device_path("Contents/A/B/2.mp3"),
                fingerprint: "fp2".into(),
            },
        ];
        store
            .save_device_manifest(&d.id, &entries)
            .expect("save manifest");
        assert_eq!(store.device_manifest(&d.id).expect("load"), entries);
    }

    #[test]
    fn delete_cascades_selection_and_manifest() {
        let store = SqliteLibraryStore::open_in_memory().expect("store");
        let d = device("dev-d");
        store.save_sync_device(&d).expect("save");
        store
            .save_device_selection(
                &d.id,
                &[PlaylistItem::Playlist(PlaylistId::new(1).expect("id"))],
            )
            .expect("sel");
        store
            .save_device_manifest(
                &d.id,
                &[SyncManifestEntry {
                    track_id: TrackId::new(1).expect("id"),
                    on_device_path: device_path("x"),
                    fingerprint: "f".into(),
                }],
            )
            .expect("manifest");

        store.delete_sync_device(&d.id).expect("delete");
        assert_eq!(store.sync_device(&d.id).expect("load"), None);
        assert!(store.device_selection(&d.id).expect("sel").is_empty());
        assert!(store.device_manifest(&d.id).expect("manifest").is_empty());
    }

    #[test]
    fn unsafe_persisted_device_sub_path_is_rejected() {
        let store = SqliteLibraryStore::open_in_memory().expect("store");
        let d = device("dev-unsafe-config");
        store.save_sync_device(&d).expect("save");
        store
            .connection_guard()
            .expect("connection")
            .execute(
                "UPDATE sync_devices SET sub_path = '../host' WHERE id = ?1",
                rusqlite::params![d.id.as_str()],
            )
            .expect("inject unsafe sub-path");

        assert!(matches!(
            store.sync_device(&d.id),
            Err(StoreError::InvalidStoredEnum(message))
                if message == "device sub-path: ../host"
        ));
    }

    #[test]
    fn unsafe_persisted_manifest_path_is_rejected() {
        let store = SqliteLibraryStore::open_in_memory().expect("store");
        let d = device("dev-unsafe-manifest");
        store.save_sync_device(&d).expect("save");
        store
            .connection_guard()
            .expect("connection")
            .execute(
                "INSERT INTO sync_manifest \
                 (device_id, track_id, on_device_path, fingerprint) \
                 VALUES (?1, 1, '../../host-file', 'fp')",
                rusqlite::params![d.id.as_str()],
            )
            .expect("inject unsafe manifest");

        assert!(matches!(
            store.device_manifest(&d.id),
            Err(StoreError::InvalidStoredEnum(message))
                if message == "device manifest path: ../../host-file"
        ));
    }
}
