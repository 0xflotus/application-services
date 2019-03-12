/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{error::*, storage::bookmarks::BookmarkRootGuid, types::SyncGuid};
use serde::{ser::SerializeStruct, Deserialize, Deserializer, Serialize, Serializer};

/// All possible fields that can appear in a bookmark record.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBookmarkItemRecord {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "parentid")]
    parent_id: Option<String>,
    has_dupe: Option<bool>,
    #[serde(rename = "parentName")]
    parent_title: Option<String>,
    date_added: Option<i64>,

    // For bookmarks, queries, folders, and livemarks.
    title: Option<String>,

    // For bookmarks and queries.
    #[serde(rename = "bmkUri")]
    url: Option<String>,

    // For bookmarks only.
    keyword: Option<String>,
    tags: Option<Vec<String>>,

    // For queries only.
    #[serde(rename = "folderName")]
    tag_folder_name: Option<String>,

    // For folders only.
    children: Option<Vec<SyncGuid>>,

    // For livemarks only.
    #[serde(rename = "feedUri")]
    feed_url: Option<String>,
    #[serde(rename = "siteUri")]
    site_url: Option<String>,

    // For separators only.
    #[serde(rename = "pos")]
    position: Option<i64>,
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct BookmarkRecord {
    // Note that `SyncGuid` does not check for validity, which is what we
    // want. If the bookmark has an invalid GUID, we'll make a new one.
    pub guid: SyncGuid,
    pub parent_guid: Option<SyncGuid>,
    pub has_dupe: bool,
    pub parent_title: Option<String>,
    pub date_added: Option<i64>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub keyword: Option<String>,
    pub tags: Vec<String>,
}

impl From<BookmarkRecord> for BookmarkItemRecord {
    fn from(b: BookmarkRecord) -> BookmarkItemRecord {
        BookmarkItemRecord::Bookmark(b)
    }
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct QueryRecord {
    pub guid: SyncGuid,
    pub parent_guid: Option<SyncGuid>,
    pub has_dupe: bool,
    pub parent_title: Option<String>,
    pub date_added: Option<i64>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub tag_folder_name: Option<String>,
}

impl From<QueryRecord> for BookmarkItemRecord {
    fn from(q: QueryRecord) -> BookmarkItemRecord {
        BookmarkItemRecord::Query(q)
    }
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct FolderRecord {
    pub guid: SyncGuid,
    pub parent_guid: Option<SyncGuid>,
    pub has_dupe: bool,
    pub parent_title: Option<String>,
    pub date_added: Option<i64>,
    pub title: Option<String>,
    pub children: Vec<SyncGuid>,
}

impl From<FolderRecord> for BookmarkItemRecord {
    fn from(f: FolderRecord) -> BookmarkItemRecord {
        BookmarkItemRecord::Folder(f)
    }
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct LivemarkRecord {
    pub guid: SyncGuid,
    pub parent_guid: Option<SyncGuid>,
    pub has_dupe: bool,
    pub parent_title: Option<String>,
    pub date_added: Option<i64>,
    pub title: Option<String>,
    pub feed_url: Option<String>,
    pub site_url: Option<String>,
}

impl From<LivemarkRecord> for BookmarkItemRecord {
    fn from(l: LivemarkRecord) -> BookmarkItemRecord {
        BookmarkItemRecord::Livemark(l)
    }
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub struct SeparatorRecord {
    pub guid: SyncGuid,
    pub parent_guid: Option<SyncGuid>,
    pub has_dupe: bool,
    pub parent_title: Option<String>,
    pub date_added: Option<i64>,
    // Not used on newer clients, but can be used to detect parent-child
    // position disagreements. Older clients use this for deduping.
    pub position: Option<i64>,
}

impl From<SeparatorRecord> for BookmarkItemRecord {
    fn from(s: SeparatorRecord) -> BookmarkItemRecord {
        BookmarkItemRecord::Separator(s)
    }
}

#[derive(Debug, Clone, Hash, PartialEq)]
pub enum BookmarkItemRecord {
    Tombstone(SyncGuid),
    Bookmark(BookmarkRecord),
    Query(QueryRecord),
    Folder(FolderRecord),
    Livemark(LivemarkRecord),
    Separator(SeparatorRecord),
}

impl BookmarkItemRecord {
    pub fn from_payload(payload: sync15::Payload) -> Result<Self> {
        let guid = payload.id.clone();
        let record = if payload.is_tombstone() {
            BookmarkItemRecord::Tombstone(guid.into())
        } else {
            payload.into_record()?
        };
        Ok(record)
    }
}

impl Serialize for BookmarkItemRecord {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("BookmarkItemRecord", 2)?;
        match self {
            BookmarkItemRecord::Tombstone(guid) => {
                state.serialize_field("id", guid_to_id(&guid))?;
                state.serialize_field("deleted", &true)?;
            }
            BookmarkItemRecord::Bookmark(b) => {
                state.serialize_field("id", guid_to_id(&b.guid))?;
                state.serialize_field("type", "bookmark")?;
                state.serialize_field("parentid", &b.parent_guid)?;
                state.serialize_field("hasDupe", &b.has_dupe)?;
                state.serialize_field("parentName", &b.parent_title)?;
                state.serialize_field("dateAdded", &b.date_added)?;
                state.serialize_field("title", &b.title)?;
                state.serialize_field("bmkUri", &b.url)?;
                state.serialize_field("keyword", &b.keyword)?;
                state.serialize_field("tags", &b.tags)?;
            }
            BookmarkItemRecord::Query(q) => {
                unimplemented!("TODO: Serialize queries");
            }
            BookmarkItemRecord::Folder(f) => {
                state.serialize_field("id", guid_to_id(&f.guid))?;
                state.serialize_field("type", "folder")?;
                state.serialize_field("parentid", &f.parent_guid)?;
                state.serialize_field("hasDupe", &f.has_dupe)?;
                state.serialize_field("parentName", &f.parent_title)?;
                state.serialize_field("dateAdded", &f.date_added)?;
                state.serialize_field("title", &f.title)?;
                state.serialize_field("children", &f.children)?;
            }
            BookmarkItemRecord::Livemark(l) => {
                unimplemented!("TODO: Serialize livemarks");
            }
            BookmarkItemRecord::Separator(s) => {
                unimplemented!("TODO: Serialize separators");
            }
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for BookmarkItemRecord {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Boilerplate to translate a synced bookmark record into a typed
        // record.
        let raw = RawBookmarkItemRecord::deserialize(d)?;
        match raw.kind.as_str() {
            "bookmark" => {
                return Ok(BookmarkRecord {
                    guid: id_to_guid(raw.id),
                    parent_guid: raw.parent_id.map(id_to_guid),
                    has_dupe: raw.has_dupe.unwrap_or(false),
                    parent_title: raw.parent_title,
                    date_added: raw.date_added,
                    title: raw.title,
                    url: raw.url,
                    keyword: raw.keyword,
                    tags: raw.tags.unwrap_or_default(),
                }
                .into());
            }
            "query" => {
                return Ok(QueryRecord {
                    guid: id_to_guid(raw.id),
                    parent_guid: raw.parent_id.map(id_to_guid),
                    has_dupe: raw.has_dupe.unwrap_or(false),
                    parent_title: raw.parent_title,
                    date_added: raw.date_added,
                    title: raw.title,
                    url: raw.url,
                    tag_folder_name: raw.tag_folder_name,
                }
                .into());
            }
            "folder" => {
                return Ok(FolderRecord {
                    guid: id_to_guid(raw.id),
                    parent_guid: raw.parent_id.map(id_to_guid),
                    has_dupe: raw.has_dupe.unwrap_or(false),
                    parent_title: raw.parent_title,
                    date_added: raw.date_added,
                    title: raw.title,
                    children: raw.children.unwrap_or_default(),
                }
                .into());
            }
            "livemark" => {
                return Ok(LivemarkRecord {
                    guid: id_to_guid(raw.id),
                    parent_guid: raw.parent_id.map(id_to_guid),
                    has_dupe: raw.has_dupe.unwrap_or(false),
                    parent_title: raw.parent_title,
                    date_added: raw.date_added,
                    title: raw.title,
                    feed_url: raw.feed_url,
                    site_url: raw.site_url,
                }
                .into());
            }
            "separator" => {
                return Ok(SeparatorRecord {
                    guid: id_to_guid(raw.id),
                    parent_guid: raw.parent_id.map(id_to_guid),
                    has_dupe: raw.has_dupe.unwrap_or(false),
                    parent_title: raw.parent_title,
                    date_added: raw.date_added,
                    position: raw.position,
                }
                .into());
            }
            _ => {}
        }
        // TODO: We don't know how to round-trip other kinds. For now, just
        // fail the sync.
        panic!("Unsupported bookmark type {}", raw.kind);
    }
}

/// Converts a Sync bookmark record ID to a Places GUID. Sync record IDs are
/// identical to Places GUIDs for all items except roots.
#[inline]
pub fn id_to_guid(id: String) -> SyncGuid {
    BookmarkRootGuid::from_sync_record_id(&id)
        .map(|g| g.as_guid())
        .unwrap_or_else(|| id.into())
}

/// Converts a Places GUID to a a Sync bookmark record ID.
#[inline]
pub fn guid_to_id(guid: &SyncGuid) -> &str {
    BookmarkRootGuid::from_guid(guid)
        .map(|g| g.as_sync_record_id())
        .unwrap_or_else(|| guid.as_ref())
}
