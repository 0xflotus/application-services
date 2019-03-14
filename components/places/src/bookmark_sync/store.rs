/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::record::{
    guid_to_id, BookmarkItemRecord, BookmarkRecord, FolderRecord, QueryRecord, SeparatorRecord, LivemarkRecord
};
use crate::error::*;
use crate::storage::{
    bookmarks::{maybe_truncate_title, BookmarkRootGuid},
    get_meta, put_meta, TAG_LENGTH_MAX, URL_LENGTH_MAX,
};
use crate::types::{
    BookmarkType, SyncGuid, SyncStatus, SyncedBookmarkKind, SyncedBookmarkValidity, Timestamp,
};
use dogear::{
    self, Content, Deletion, IntoTree, Item, LogLevel, MergedDescendant, Tree, UploadReason,
};
use lazy_static::lazy_static;
use rusqlite::{Connection, NO_PARAMS};
use sql_support::{self, ConnExt};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::result;
use std::time::Duration;
use sync15::{
    telemetry, ClientInfo, CollectionRequest, IncomingChangeset, OutgoingChangeset, Payload,
    ServerTimestamp, Store,
};
use url::Url;

static LAST_SYNC_META_KEY: &'static str = "bookmarks_last_sync_time";

// From Desktop's Ci.nsINavHistoryQueryOptions, but we define it as a str
// as that's how we use it here.
const RESULTS_AS_TAG_CONTENTS: &str = "7";

lazy_static! {
    static ref LOCAL_ROOTS_AS_SQL_SET: String = {
        // phew - this seems more complicated then it should be.
        let roots_as_strings: Vec<String> = BookmarkRootGuid::user_roots().iter().map(|g| format!("'{}'", g.as_guid())).collect();
        roots_as_strings.join(",")
    };

    static ref LOCAL_ITEMS_SQL_FRAGMENT: String = {
        format!(
            "localItems(id, guid, parentId, parentGuid, position, type, title,
                     parentTitle, placeId, dateAdded, lastModified, syncChangeCounter,
                     isSyncable, level) AS (
            SELECT b.id, b.guid, p.id, p.guid, b.position, b.type, b.title, p.title,
                   b.fk, b.dateAdded, b.lastModified, b.syncChangeCounter,
                   b.guid IN ({user_content_roots}), 0
            FROM moz_bookmarks b
            JOIN moz_bookmarks p ON p.id = b.parent
            WHERE b.guid <> '{tags_guid}' AND
                  p.guid = '{root_guid}'
            UNION ALL
            SELECT b.id, b.guid, s.id, s.guid, b.position, b.type, b.title, s.title,
                   b.fk, b.dateAdded, b.lastModified, b.syncChangeCounter,
                   s.isSyncable, s.level + 1
            FROM moz_bookmarks b
            JOIN localItems s ON s.id = b.parent
            WHERE b.guid <> '{root_guid}')",
            user_content_roots = *LOCAL_ROOTS_AS_SQL_SET,
            root_guid = BookmarkRootGuid::Root.as_guid().as_ref(),
            tags_guid = "_tags_" // XXX - need tags!
        )
    };
}

fn validate_tag(tag: &Option<String>) -> Option<&str> {
    match tag {
        None => None,
        Some(t) => {
            // Drop empty and oversized tags.
            let t = t.trim();
            if t.len() == 0 || t.len() > TAG_LENGTH_MAX {
                None
            } else {
                Some(t)
            }
        }
    }
}

pub struct BookmarksStore<'a> {
    pub db: &'a Connection,
    pub client_info: &'a Cell<Option<ClientInfo>>,
    local_time: Timestamp,
    remote_time: ServerTimestamp,
}

struct Driver;

impl dogear::Driver for Driver {
    fn generate_new_guid(&self, _invalid_guid: &dogear::Guid) -> dogear::Result<dogear::Guid> {
        Ok(SyncGuid::new().into())
    }

    fn log_level(&self) -> LogLevel {
        LogLevel::Silent
    }

    fn log(&self, _level: LogLevel, _args: fmt::Arguments) {}
}

impl<'a> dogear::Store<Error> for BookmarksStore<'a> {
    /// Builds a fully rooted, consistent tree from all local items and
    /// tombstones.
    fn fetch_local_tree(&self) -> Result<Tree> {
        let mut builder = Tree::with_root(Item::root());

        let sql = format!(
            r#"
            WITH RECURSIVE
            {local_items_fragment}
            SELECT s.id, s.guid, s.parentGuid, {kind} AS kind,
                   s.lastModified / 1000 AS localModified, s.syncChangeCounter
            FROM localItems s
            ORDER BY s.level, s.parentId, s.position"#,
            local_items_fragment = *LOCAL_ITEMS_SQL_FRAGMENT,
            kind = type_to_kind("s.type", UrlOrPlaceId::PlaceId("s.placeId")),
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let guid = row.get_checked::<_, SyncGuid>("guid")?;
            let kind = SyncedBookmarkKind::from_u8(row.get_checked("kind")?)?;
            let mut item = Item::new(guid.into(), kind.into());
            // Note that this doesn't account for local clock skew.
            let age = row
                .get_checked::<_, Timestamp>("localModified")
                .unwrap_or_default()
                .duration_since(self.local_time)
                .unwrap_or_default();
            item.age = age.as_secs() as i64 * 1000 + i64::from(age.subsec_millis());
            item.needs_merge = row.get_checked::<_, u32>("syncChangeCounter")? > 0;
            let parent_guid = row.get_checked::<_, SyncGuid>("parentGuid")?;
            builder.item(item)?.by_structure(&parent_guid.into())?;
        }

        let mut tree = builder.into_tree()?;

        // Note tombstones for locally deleted items.
        let mut stmt = self.db.prepare("SELECT guid FROM moz_bookmarks_deleted")?;
        let rows = stmt.query_and_then(NO_PARAMS, |row| row.get_checked::<_, SyncGuid>("guid"))?;
        for row in rows {
            let guid = row?;
            tree.note_deleted(guid.into());
        }

        Ok(tree)
    }

    /// Fetches content info for all "new" and "unknown" local items that
    /// haven't been synced. We'll try to dedupe them to changed remote items
    /// with similar contents and different GUIDs.
    fn fetch_new_local_contents(&self) -> Result<HashMap<dogear::Guid, Content>> {
        let mut contents = HashMap::new();

        let sql = format!(
            r#"
            SELECT b.guid, b.type, IFNULL(b.title, "") AS title, h.url,
                   b.position
            FROM moz_bookmarks b
            JOIN moz_bookmarks p ON p.id = b.parent
            LEFT JOIN moz_places h ON h.id = b.fk
            LEFT JOIN moz_bookmarks_synced v ON v.guid = b.guid
            WHERE v.guid IS NULL AND
                  p.guid <> '{root_guid}' AND
                  b.syncStatus <> {sync_status}"#,
            root_guid = BookmarkRootGuid::Root.as_guid().as_ref(),
            sync_status = SyncStatus::Normal as u8
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let typ = match BookmarkType::from_u8(row.get_checked("type")?) {
                Some(t) => t,
                None => continue,
            };
            let content = match typ {
                BookmarkType::Bookmark => {
                    let title = row.get_checked("title")?;
                    let url_href = row.get_checked("url")?;
                    Content::Bookmark { title, url_href }
                }
                BookmarkType::Folder => {
                    let title = row.get_checked("title")?;
                    Content::Folder { title }
                }
                BookmarkType::Separator => {
                    let position = row.get_checked("position")?;
                    Content::Separator { position }
                }
            };
            let guid = row.get_checked::<_, SyncGuid>("guid")?;
            contents.insert(guid.into(), content);
        }

        Ok(contents)
    }

    /// Builds a fully rooted tree from all synced items and tombstones.
    fn fetch_remote_tree(&self) -> Result<Tree> {
        let mut builder = Tree::with_root(Item::root());

        let sql = format!(
            "
            SELECT guid, parentGuid, serverModified, kind, needsMerge, validity
            FROM moz_bookmarks_synced
            WHERE NOT isDeleted AND
                  guid <> '{root_guid}'",
            root_guid = BookmarkRootGuid::Root.as_guid().as_ref()
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let guid = row.get_checked::<_, SyncGuid>("guid")?;
            let kind = SyncedBookmarkKind::from_u8(row.get_checked("kind")?)?;
            let mut item = Item::new(guid.into(), kind.into());
            let age = ServerTimestamp(row.get_checked::<_, f64>("serverModified").unwrap_or(0f64))
                .duration_since(self.remote_time)
                .unwrap_or_default();
            item.age = age.as_secs() as i64 * 1000 + i64::from(age.subsec_millis());
            item.needs_merge = row.get_checked("needsMerge")?;
            item.validity = SyncedBookmarkValidity::from_u8(row.get_checked("validity")?)?.into();

            let p = builder.item(item)?;
            if let Some(parent_guid) = row.get_checked::<_, Option<SyncGuid>>("parentGuid")? {
                p.by_parent_guid(parent_guid.into())?;
            }
        }

        let sql = format!(
            "
            SELECT guid, parentGuid FROM moz_bookmarks_synced_structure
            WHERE guid <> '{root_guid}'
            ORDER BY parentGuid, position",
            root_guid = BookmarkRootGuid::Root.as_guid().as_ref()
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let guid = row.get_checked::<_, SyncGuid>("guid")?;
            let parent_guid = row.get_checked::<_, SyncGuid>("parentGuid")?;
            builder
                .parent_for(&guid.into())
                .by_children(&parent_guid.into())?;
        }

        let mut tree = builder.into_tree()?;

        // Note tombstones for remotely deleted items.
        let mut stmt = self
            .db
            .prepare("SELECT guid FROM moz_bookmarks_synced WHERE isDeleted AND needsMerge")?;
        let rows = stmt.query_and_then(NO_PARAMS, |row| row.get_checked::<_, SyncGuid>("guid"))?;
        for row in rows {
            let guid = row?;
            tree.note_deleted(guid.into());
        }

        Ok(tree)
    }

    /// Fetches content info for all synced items that changed since the last
    /// sync and don't exist locally.
    fn fetch_new_remote_contents(&self) -> Result<HashMap<dogear::Guid, Content>> {
        let mut contents = HashMap::new();

        let sql = format!(
            r#"
            SELECT v.guid, v.kind, IFNULL(v.title, "") AS title, h.url,
                   s.position
            FROM moz_bookmarks_synced v
            JOIN moz_bookmarks_synced_structure s ON s.guid = v.guid
            LEFT JOIN moz_places h ON h.id = v.placeId
            LEFT JOIN moz_bookmarks b ON b.guid = v.guid
            WHERE NOT v.isDeleted AND
                  v.needsMerge AND
                  b.guid IS NULL AND
                  s.parentGuid <> '{root_guid}'"#,
            root_guid = BookmarkRootGuid::Root.as_guid().as_ref()
        );
        let mut stmt = self.db.prepare(&sql)?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let content = match SyncedBookmarkKind::from_u8(row.get_checked("kind")?)? {
                SyncedBookmarkKind::Bookmark | SyncedBookmarkKind::Query => {
                    let title = row.get_checked("title")?;
                    let url_href = row.get_checked("url")?;
                    Content::Bookmark { title, url_href }
                }
                SyncedBookmarkKind::Folder => {
                    let title = row.get_checked("title")?;
                    Content::Folder { title }
                }
                SyncedBookmarkKind::Separator => {
                    let position = row.get_checked("position")?;
                    Content::Separator { position }
                }
                _ => continue,
            };
            let guid = row.get_checked::<_, SyncGuid>("guid")?;
            contents.insert(guid.into(), content);
        }

        Ok(contents)
    }

    fn apply<'t>(
        &self,
        descendants: Vec<MergedDescendant<'t>>,
        deletions: Vec<Deletion>,
    ) -> Result<()> {
        if !self.has_changes()? {
            return Ok(());
        }
        let tx = self.db.unchecked_transaction()?;
        let result = self
            .update_local_items(descendants, deletions)
            .and_then(|_| self.stage_local_items_to_upload())
            .and_then(|_| {
                self.db.execute_batch(
                    "
                    DELETE FROM mergedTree;
                    DELETE FROM idsToWeaklyUpload;",
                )?;
                Ok(())
            });
        match result {
            Ok(_) => tx.commit()?,
            Err(_) => tx.rollback()?,
        }
        result
    }
}

impl<'a> BookmarksStore<'a> {
    fn store_incoming_bookmark(&self, modified: ServerTimestamp, b: BookmarkRecord) -> Result<()> {
        let (url, validity) = match self.maybe_store_href(b.url.as_ref()) {
            Ok(url) => (Some(url.into_string()), SyncedBookmarkValidity::Valid),
            Err(e) => {
                log::warn!("Incoming bookmark has an invalid URL: {:?}", e);
                (None, SyncedBookmarkValidity::Replace)
            }
        };
        self.db.execute_named_cached(
            r#"REPLACE INTO moz_bookmarks_synced(guid, parentGuid, serverModified, needsMerge, kind,
                                                 dateAdded, title, keyword, validity, placeId)
               VALUES(:guid, :parentGuid, :serverModified, 1, :kind,
                      :dateAdded, NULLIF(:title, ""), :keyword, :validity,
                      -- XXX - when url is null we still fail below when we call hash()???
                      CASE WHEN :url ISNULL
                      THEN NULL
                      ELSE (SELECT id FROM moz_places
                            WHERE url_hash = hash(:url) AND
                            url = :url)
                      END
                      )"#,
            &[
                (":guid", &b.guid.as_ref()),
                (":parentGuid", &b.parent_guid.as_ref()),
                (":serverModified", &(modified.as_millis() as i64)),
                (":kind", &SyncedBookmarkKind::Bookmark),
                (":dateAdded", &b.date_added),
                (":title", &maybe_truncate_title(&b.title)),
                (":keyword", &b.keyword),
                (":validity", &validity),
                (":url", &url),
            ],
        )?;
        Ok(())
    }

    fn store_incoming_folder(&self, modified: ServerTimestamp, b: FolderRecord) -> Result<()> {
        self.db.execute_named_cached(
            r#"REPLACE INTO moz_bookmarks_synced(guid, parentGuid, serverModified, needsMerge, kind,
                                                 dateAdded, title)
               VALUES(:guid, :parentGuid, :serverModified, 1, :kind,
                      :dateAdded, NULLIF(:title, ""))"#,
            &[
                (":guid", &b.guid.as_ref()),
                (":parentGuid", &b.parent_guid.as_ref()),
                (":serverModified", &(modified.as_millis() as i64)),
                (":kind", &SyncedBookmarkKind::Folder),
                (":dateAdded", &b.date_added),
                (":title", &maybe_truncate_title(&b.title)),
            ],
        )?;
        for (position, child_guid) in b.children.iter().enumerate() {
            self.db.execute_named_cached(
                "REPLACE INTO moz_bookmarks_synced_structure(guid, parentGuid, position)
                 VALUES(:guid, :parentGuid, :position)",
                &[
                    (":guid", &child_guid),
                    (":parentGuid", &b.guid.as_ref()),
                    (":position", &(position as i64)),
                ],
            )?;
        }
        Ok(())
    }

    fn store_incoming_tombstone(&self, modified: ServerTimestamp, guid: &SyncGuid) -> Result<()> {
        self.db.execute_named_cached(
            r#"REPLACE INTO moz_bookmarks_synced(guid, parentGuid, serverModified, needsMerge,
                                                 dateAdded, isDeleted)
               VALUES(:guid, NULL, :serverModified, 1, 0, 1)"#,
            &[
                (":guid", guid),
                (":serverModified", &(modified.as_millis() as i64)),
            ],
        )?;
        Ok(())
    }

    fn determine_query_url_and_validity(
        &self,
        q: &QueryRecord,
        url: Url,
    ) -> Result<(Option<Url>, SyncedBookmarkValidity)> {
        // wow - this  is complex, but markh is struggling to see how to
        // improve it
        let (maybe_url, validity) = {
            // If the URL has `type={RESULTS_AS_TAG_CONTENTS}` then we
            // rewrite the URL as `place:tag=...`
            // Sadly we can't use `url.query_pairs()` here as the format of
            // the url is, eg, `place:type=7` - ie, the "params" are actually
            // the path portion of the URL.
            let parse = url::form_urlencoded::parse(&url.path().as_bytes());
            if parse
                .clone()
                .any(|(k, v)| k == "type" && v == RESULTS_AS_TAG_CONTENTS)
            {
                if let Some(tag) = validate_tag(&q.tag_folder_name) {
                    (
                        Some(Url::parse(&format!("place:tag={}", tag))?),
                        SyncedBookmarkValidity::Reupload,
                    )
                } else {
                    (None, SyncedBookmarkValidity::Replace)
                }
            } else {
                // If we have `folder=...` the folder value is a row_id
                // from desktop, so useless to us - so we append `&excludeItems=1`
                // if it isn't already there.
                if parse.clone().any(|(k, _)| k == "folder") {
                    if parse.clone().any(|(k, v)| k == "excludeItems" && v == "1") {
                        (Some(url), SyncedBookmarkValidity::Valid)
                    } else {
                        // need to add excludeItems, and I guess we should do
                        // it properly without resorting to string manipulation...
                        let tail = url::form_urlencoded::Serializer::new(String::new())
                            .extend_pairs(parse.clone())
                            .append_pair("excludeItems", "1")
                            .finish();
                        (
                            Some(Url::parse(&format!("place:{}", tail))?),
                            SyncedBookmarkValidity::Reupload,
                        )
                    }
                } else {
                    // it appears to be fine!
                    (Some(url), SyncedBookmarkValidity::Valid)
                }
            }
        };
        Ok(match self.maybe_store_url(maybe_url) {
            Ok(url) => (Some(url), validity),
            Err(e) => {
                log::warn!("query {} has invalid URL '{:?}'", q.guid, q.url);
                (None, SyncedBookmarkValidity::Replace)
            }
        })
    }

    fn store_incoming_query(&self, modified: ServerTimestamp, q: QueryRecord) -> Result<()> {
        let (url, validity) = match q.url.as_ref().and_then(|href| Url::parse(href).ok()) {
            Some(url) => self.determine_query_url_and_validity(&q, url)?,
            None => {
                log::warn!("query {} has invalid URL '{:?}'", q.guid, q.url);
                (None, SyncedBookmarkValidity::Replace)
            }
        };

        self.db.execute_named_cached(
            r#"REPLACE INTO moz_bookmarks_synced(guid, parentGuid, serverModified, needsMerge, kind,
                                                 dateAdded, title, validity, placeId)
               VALUES(:guid, :parentGuid, :serverModified, 1, :kind,
                      :dateAdded, NULLIF(:title, ""), :validity,
                      (SELECT id FROM moz_places
                            WHERE url_hash = hash(:url) AND
                            url = :url
                      )
                     )"#,
            &[
                (":guid", &q.guid.as_ref()),
                (":parentGuid", &q.parent_guid.as_ref()),
                (":serverModified", &(modified.as_millis() as i64)),
                (":kind", &SyncedBookmarkKind::Query),
                (":dateAdded", &q.date_added),
                (":title", &maybe_truncate_title(&q.title)),
                (":validity", &validity),
                (":url", &url.map(|u| u.into_string()))
            ],
        )?;
        Ok(())
    }

    fn store_incoming_livemark(&self, modified: ServerTimestamp, l: LivemarkRecord) -> Result<()> {
        // livemarks don't store a reference to the place, so we validate it manually.
        fn validate_href(h: Option<String>, guid: &SyncGuid, what: &str) -> Option<String> {
            match h {
                Some(h) => match Url::parse(&h) {
                    Ok(url) => {
                        let s = url.to_string();
                        if s.len() > URL_LENGTH_MAX {
                            log::warn!("Livemark {} has a {} URL which is too long", &guid, what);
                            None
                        } else {
                            Some(s)
                        }
                    },
                    Err(e) => {
                        log::warn!("Livemark {} has an invalid {} URL '{}'", &guid, what, h);
                        None
                    }
                },
                None => {
                    log::warn!("Livemark {} has no {} URL", &guid, what);
                    None
                }
            }
        }
        let feed_url = validate_href(l.feed_url, &l.guid, "feed");
        let site_url = validate_href(l.site_url, &l.guid, "site");
        let validity = if feed_url.is_some() {
            SyncedBookmarkValidity::Valid
        } else {
            SyncedBookmarkValidity::Replace

        };
        self.db.execute_named_cached(
            r#"REPLACE INTO moz_bookmarks_synced(guid, parentGuid, serverModified, needsMerge, kind,
                                                 dateAdded, title, feedURL, siteURL, validity)
               VALUES(:guid, :parentGuid, :serverModified, 1, :kind,
                      :dateAdded, :title, :feedUrl, :siteUrl, :validity)"#,
            &[
                (":guid", &l.guid.as_ref()),
                (":parentGuid", &l.parent_guid.as_ref()),
                (":serverModified", &(modified.as_millis() as i64)),
                (":kind", &SyncedBookmarkKind::Livemark),
                (":dateAdded", &l.date_added),
                (":title", &l.title),
                (":feedUrl", &feed_url),
                (":siteUrl", &site_url),
                (":validity", &validity),
            ],
        )?;
        Ok(())
    }

    fn store_incoming_sep(&self, modified: ServerTimestamp, s: SeparatorRecord) -> Result<()> {
        self.db.execute_named_cached(
            r#"REPLACE INTO moz_bookmarks_synced(guid, parentGuid, serverModified, needsMerge, kind,
                                                 dateAdded)
               VALUES(:guid, :parentGuid, :serverModified, 1, :kind,
                      :dateAdded)"#,
            &[
                (":guid", &s.guid.as_ref()),
                (":parentGuid", &s.parent_guid.as_ref()),
                (":serverModified", &(modified.as_millis() as i64)),
                (":kind", &SyncedBookmarkKind::Separator),
                (":dateAdded", &s.date_added),
            ],
        )?;
        Ok(())
    }

    fn maybe_store_href(&self, href: Option<&String>) -> Result<Url> {
        if let Some(href) = href {
            self.maybe_store_url(Some(Url::parse(href)?))
        } else {
            self.maybe_store_url(None)
        }
    }

    fn maybe_store_url(&self, url: Option<Url>) -> Result<Url> {
        if let Some(url) = url {
            if url.as_str().len() > URL_LENGTH_MAX {
                return Err(ErrorKind::InvalidPlaceInfo(InvalidPlaceInfo::UrlTooLong).into());
            }
            self.db.execute_named_cached(
                "INSERT OR IGNORE INTO moz_places(guid, url, url_hash)
                 VALUES(IFNULL((SELECT guid FROM moz_places
                                WHERE url_hash = hash(:url) AND
                                      url = :url),
                        generate_guid()), :url, hash(:url))",
                &[(":url", &url.as_str())],
            )?;
            Ok(url)
        } else {
            Err(ErrorKind::InvalidPlaceInfo(InvalidPlaceInfo::NoUrl).into())
        }
    }

    pub fn apply_payload(
        &self,
        timestamp: ServerTimestamp,
        payload: sync15::Payload,
    ) -> Result<()> {
        let item = BookmarkItemRecord::from_payload(payload)?;
        match item {
            BookmarkItemRecord::Tombstone(guid) => {
                self.store_incoming_tombstone(timestamp, &guid)?
            }
            BookmarkItemRecord::Bookmark(b) => self.store_incoming_bookmark(timestamp, b)?,
            BookmarkItemRecord::Query(q) => self.store_incoming_query(timestamp, q)?,
            BookmarkItemRecord::Folder(f) => self.store_incoming_folder(timestamp, f)?,
            BookmarkItemRecord::Livemark(l) => self.store_incoming_livemark(timestamp, l)?,
            BookmarkItemRecord::Separator(s) => self.store_incoming_sep(timestamp, s)?,
        }
        Ok(())
    }

    fn has_changes(&self) -> Result<bool> {
        // In the first subquery, we check incoming items with needsMerge = true
        // except the tombstones who don't correspond to any local bookmark because
        // we don't store them yet, hence never "merged" (see bug 1343103).
        let sql = format!(
            "
            SELECT
              EXISTS (
               SELECT 1
               FROM moz_bookmarks_synced v
               LEFT JOIN moz_bookmarks b ON v.guid = b.guid
               WHERE v.needsMerge AND
               (NOT v.isDeleted OR b.guid NOT NULL)
              ) OR EXISTS (
               WITH RECURSIVE
               {}
               SELECT 1
               FROM localItems
               WHERE syncChangeCounter > 0
              ) OR EXISTS (
               SELECT 1
               FROM moz_bookmarks_deleted
              )
              AS hasChanges
        ",
            *LOCAL_ITEMS_SQL_FRAGMENT
        );
        Ok(self
            .db
            .try_query_row(
                &sql,
                &[],
                |row| -> rusqlite::Result<_> { Ok(row.get_checked::<_, bool>(0)?) },
                false,
            )?
            .unwrap_or(false))
    }

    /// Builds a temporary table with the merge states of all nodes in the merged
    /// tree, then updates the local tree to match the merged tree.
    ///
    /// Conceptually, we examine the merge state of each item, and either leave the
    /// item unchanged, upload the local side, apply the remote side, or apply and
    /// then reupload the remote side with a new structure.
    fn update_local_items<'t>(
        &self,
        descendants: Vec<MergedDescendant<'t>>,
        deletions: Vec<Deletion>,
    ) -> Result<()> {
        // First, insert rows for all merged descendants.
        sql_support::each_sized_chunk(
            &descendants,
            sql_support::default_max_variable_number() / 4,
            |chunk, _| -> Result<()> {
                // We can't avoid allocating here, since we're binding four
                // parameters per descendant. Rust's `SliceConcatExt::concat`
                // is semantically equivalent, but requires a second allocation,
                // which we _can_ avoid.
                let mut params = Vec::with_capacity(chunk.len() * 4);
                for d in chunk.iter() {
                    params.push(
                        d.merged_node
                            .merge_state
                            .local_node()
                            .map(|node| node.guid.as_str()),
                    );
                    params.push(
                        d.merged_node
                            .merge_state
                            .remote_node()
                            .map(|node| node.guid.as_str()),
                    );
                    params.push(Some(d.merged_node.guid.as_str()));
                    params.push(Some(d.merged_parent_node.guid.as_str()));
                }
                self.db.execute(&format!(
                    "
                    INSERT INTO mergedTree(localGuid, remoteGuid, mergedGuid, mergedParentGuid, level,
                                           position, useRemote, shouldUpload)
                    VALUES {}",
                    sql_support::repeat_display(chunk.len(), ",", |index, f| {
                        let d = &chunk[index];
                        write!(f, "(?, ?, ?, ?, {}, {}, {}, {})",
                            d.level, d.position, d.merged_node.merge_state.should_apply(),
                            d.merged_node.merge_state.upload_reason() != UploadReason::None)
                    })
                ), &params)?;
                Ok(())
            },
        )?;

        // Next, insert rows for deletions. Unlike Desktop, there's no
        // `noteItemRemoved` trigger on `itemsToRemove`, since we don't support
        // observer notifications.
        sql_support::each_chunk(&deletions, |chunk, _| -> Result<()> {
            self.db.execute(
                &format!(
                    "
                    INSERT INTO itemsToRemove(guid, localLevel, shouldUploadTombstone)
                    VALUES {}",
                    sql_support::repeat_display(chunk.len(), ",", |index, f| {
                        let d = &chunk[index];
                        write!(f, "(?, {}, {})", d.local_level, d.should_upload_tombstone)
                    })
                ),
                chunk.iter().map(|d| d.guid.as_str()),
            )?;
            Ok(())
        })?;

        // "Deleting" from `itemsToMerge` fires the `insertNewLocalItems` and
        // `updateExistingLocalItems` triggers.
        self.db.execute_batch("DELETE FROM itemsToMerge")?;

        // "Deleting" from `structureToMerge` fires the `updateLocalStructure`
        // trigger.
        self.db.execute_batch("DELETE FROM structureToMerge")?;

        self.db.execute_batch("DELETE FROM itemsToRemove")?;

        self.db.execute_batch("DELETE FROM relatedIdsToReupload")?;

        Ok(())
    }

    /// Stores a snapshot of all locally changed items in a temporary table for
    /// upload. This is called from within the merge transaction, to ensure that
    /// changes made during the sync don't cause us to upload inconsistent
    /// records.
    ///
    /// Conceptually, `itemsToUpload` is a transient "view" of locally changed
    /// items. The local change counter is the persistent record of items that
    /// we need to upload, so, if upload is interrupted or fails, we'll stage
    /// the items again on the next sync.
    fn stage_local_items_to_upload(&self) -> Result<()> {
        // Stage remotely changed items with older local creation dates. These are
        // tracked "weakly": if the upload is interrupted or fails, we won't
        // reupload the record on the next sync.
        self.db.execute_batch(
            r#"
            INSERT OR IGNORE INTO idsToWeaklyUpload(id)
            SELECT b.id FROM moz_bookmarks b
            JOIN mergedTree r ON r.mergedGuid = b.guid
            JOIN moz_bookmarks_synced v ON v.guid = r.remoteGuid
            WHERE r.useRemote AND
                  /* "b.dateAdded" is in microseconds; "v.dateAdded" is in
                     milliseconds. */
                  b.dateAdded < v.dateAdded"#,
        )?;

        // Stage remaining locally changed items for upload.
        self.db.execute_batch(&format!(
            "
            WITH RECURSIVE
            {local_items_fragment}
            INSERT INTO itemsToUpload(id, guid, syncChangeCounter, parentGuid,
                                      parentTitle, dateAdded, title, placeId,
                                      kind, url, keyword, position,
                                      tagFolderName)
            SELECT s.id, s.guid, s.syncChangeCounter, s.parentGuid,
                   s.parentTitle, s.dateAdded, s.title, s.placeId,
                   {kind}, h.url, NULL AS keyword, s.position,
                   NULL AS tagFolderName
            FROM localItems s
            LEFT JOIN moz_places h ON h.id = s.placeId
            LEFT JOIN idsToWeaklyUpload w ON w.id = s.id
            WHERE s.syncChangeCounter >= 1 OR
                  w.id NOT NULL",
            local_items_fragment = *LOCAL_ITEMS_SQL_FRAGMENT,
            kind = type_to_kind("s.type", UrlOrPlaceId::Url("h.url")),
        ))?;

        // Record the child GUIDs of locally changed folders, which we use to
        // populate the `children` array in the record.
        self.db.execute_batch(
            "
            INSERT INTO structureToUpload(guid, parentId, position)
            SELECT b.guid, b.parent, b.position FROM moz_bookmarks b
            JOIN itemsToUpload o ON o.id = b.parent",
        )?;

        // Finally, stage tombstones for deleted items.
        self.db.execute_batch(
            "
            INSERT OR IGNORE INTO itemsToUpload(guid, syncChangeCounter, isDeleted)
            SELECT guid, 1, 1 FROM moz_bookmarks_deleted",
        )?;

        Ok(())
    }

    /// Inflates Sync records for all staged outgoing items.
    fn fetch_outgoing_records(&self, timestamp: ServerTimestamp) -> Result<OutgoingChangeset> {
        let mut outgoing = OutgoingChangeset::new(self.collection_name().into(), timestamp);
        let mut child_guids_by_local_parent_id: HashMap<i64, Vec<SyncGuid>> = HashMap::new();

        let mut stmt = self.db.prepare(
            "SELECT parentId, guid FROM structureToUpload
             ORDER BY parentId, position",
        )?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let local_parent_id = row.get_checked::<_, i64>("parentId")?;
            let child_guid = row.get_checked::<_, SyncGuid>("guid")?;
            let child_guids = child_guids_by_local_parent_id
                .entry(local_parent_id)
                .or_default();
            child_guids.push(child_guid);
        }

        let mut stmt = self.db.prepare(
            r#"SELECT id, syncChangeCounter, guid, isDeleted, kind,
                      tagFolderName, keyword, url, IFNULL(title, "") AS title,
                      position, parentGuid,
                      IFNULL(parentTitle, "") AS parentTitle, dateAdded
               FROM itemsToUpload"#,
        )?;
        let mut results = stmt.query(NO_PARAMS)?;
        while let Some(result) = results.next() {
            let row = result?;
            let guid = row.get_checked::<_, SyncGuid>("guid")?;
            let is_deleted = row.get_checked::<_, bool>("isDeleted")?;
            if is_deleted {
                outgoing
                    .changes
                    .push(Payload::new_tombstone(guid_to_id(&guid).into()));
                continue;
            }
            let parent_guid = row.get_checked::<_, SyncGuid>("parentGuid")?;
            let parent_title = row.get_checked::<_, String>("parentTitle")?;
            let date_added = row.get_checked::<_, i64>("dateAdded")?;
            let record: BookmarkItemRecord =
                match SyncedBookmarkKind::from_u8(row.get_checked("kind")?)? {
                    SyncedBookmarkKind::Bookmark => {
                        let title = row.get_checked::<_, String>("title")?;
                        let url = row.get_checked::<_, String>("url")?;
                        BookmarkRecord {
                            guid,
                            parent_guid: Some(parent_guid),
                            has_dupe: true,
                            parent_title: Some(parent_title),
                            date_added: Some(date_added),
                            title: Some(title),
                            url: Some(url),
                            keyword: None,
                            tags: Vec::new(),
                        }
                        .into()
                    }
                    SyncedBookmarkKind::Query => {
                        let title = row.get_checked::<_, String>("title")?;
                        let url = row.get_checked::<_, String>("url")?;
                        QueryRecord {
                            guid,
                            parent_guid: Some(parent_guid),
                            has_dupe: true,
                            parent_title: Some(parent_title),
                            date_added: Some(date_added),
                            title: Some(title),
                            url: Some(url),
                            tag_folder_name: None,
                        }
                        .into()
                    }
                    SyncedBookmarkKind::Folder => {
                        let title = row.get_checked::<_, String>("title")?;
                        let local_id = row.get_checked::<_, i64>("id")?;
                        let children = child_guids_by_local_parent_id
                            .remove(&local_id)
                            .unwrap_or_default();
                        FolderRecord {
                            guid,
                            parent_guid: Some(parent_guid),
                            has_dupe: true,
                            parent_title: Some(parent_title),
                            date_added: Some(date_added),
                            title: Some(title),
                            children,
                        }
                        .into()
                    }
                    SyncedBookmarkKind::Livemark => continue,
                    SyncedBookmarkKind::Separator => {
                        let position = row.get_checked::<_, i64>("position")?;
                        SeparatorRecord {
                            guid,
                            parent_guid: Some(parent_guid),
                            has_dupe: true,
                            parent_title: Some(parent_title),
                            date_added: Some(date_added),
                            position: Some(position),
                        }
                        .into()
                    }
                };
            outgoing.changes.push(Payload::from_record(record)?);
        }

        Ok(outgoing)
    }

    /// Decrements the change counter, updates the sync status, and cleans up
    /// tombstones for successfully synced items. Sync calls this method at the
    /// end of each bookmark sync.
    fn push_synced_items(&self, uploaded_at: ServerTimestamp, guids: &[String]) -> Result<()> {
        // Flag all successfully synced records as uploaded. This `UPDATE` fires
        // the `pushUploadedChanges` trigger, which updates local change
        // counters and writes the items back to the synced bookmarks table.
        sql_support::each_chunk(&guids, |chunk, _| -> Result<()> {
            self.db.execute(
                &format!(
                    "UPDATE itemsToUpload SET
                       uploadedAt = {uploaded_at}
                     WHERE guid IN ({values})",
                    uploaded_at = uploaded_at.as_millis(),
                    values = sql_support::repeat_sql_values(chunk.len())
                ),
                chunk,
            )?;
            Ok(())
        })?;

        // Fast-forward the last sync time, so that we don't download the
        // records we just uploaded on the next sync.
        put_meta(
            self.db,
            LAST_SYNC_META_KEY,
            &(uploaded_at.as_millis() as i64),
        )?;

        // Clean up.
        self.db.execute_batch("DELETE FROM itemsToUpload")?;

        Ok(())
    }
}

impl<'a> Store for BookmarksStore<'a> {
    #[inline]
    fn collection_name(&self) -> &'static str {
        "bookmarks"
    }

    fn apply_incoming(
        &self,
        inbound: IncomingChangeset,
        incoming_telemetry: &mut telemetry::EngineIncoming,
    ) -> result::Result<OutgoingChangeset, failure::Error> {
        use dogear::Store;

        // Stage all incoming items.
        let timestamp = inbound.timestamp;
        let mut tx = self
            .db
            .time_chunked_transaction(Duration::from_millis(1000))?;
        for incoming in inbound.changes {
            self.apply_payload(timestamp, incoming.0)?;
            incoming_telemetry.applied(1);
            tx.maybe_commit()?;
        }
        tx.commit()?;

        // write the timestamp now, so if we are interrupted merging or
        // creating outgoing changesets we don't need to re-download the same
        // records.
        put_meta(self.db, LAST_SYNC_META_KEY, &(timestamp.as_millis() as i64))?;

        // Merge and stage outgoing items.
        self.merge_with_driver(&Driver)?;

        let outgoing = self.fetch_outgoing_records(inbound.timestamp)?;
        Ok(outgoing)
    }

    fn sync_finished(
        &self,
        new_timestamp: ServerTimestamp,
        records_synced: &[String],
    ) -> result::Result<(), failure::Error> {
        let tx = self.db.unchecked_transaction()?;
        let result = self.push_synced_items(new_timestamp, records_synced);
        match result {
            Ok(_) => tx.commit()?,
            Err(_) => tx.rollback()?,
        }
        result?;
        Ok(())
    }

    fn get_collection_request(&self) -> result::Result<CollectionRequest, failure::Error> {
        let since = get_meta::<i64>(self.db, LAST_SYNC_META_KEY)?
            .map(|millis| ServerTimestamp(millis as f64 / 1000.0))
            .unwrap_or_default();
        Ok(CollectionRequest::new(self.collection_name())
            .full()
            .newer_than(since))
    }

    fn reset(&self) -> result::Result<(), failure::Error> {
        unimplemented!("TODO: Wipe staged items and reset sync statuses");
    }

    fn wipe(&self) -> result::Result<(), failure::Error> {
        log::warn!("not implemented");
        Ok(())
    }
}

fn type_to_kind<'a>(typ: &'a str, url: UrlOrPlaceId<'a>) -> TypeToKind<'a> {
    TypeToKind { typ, url }
}

/// A helper that interpolates a SQL expression for converting Places item types
/// to Sync record kinds. `typ` is the name of the bookmark type column in the
/// projection.
struct TypeToKind<'a> {
    typ: &'a str,
    url: UrlOrPlaceId<'a>,
}

impl<'a> fmt::Display for TypeToKind<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            r#"(CASE {typ}
                WHEN {bookmark_type} THEN (
                    CASE substr({url}, 1, 6)
                    /* Queries are bookmarks with a "place:" URL scheme. */
                    WHEN 'place:' THEN {query_kind}
                    ELSE {bookmark_kind}
                    END
                )
                WHEN {folder_type} THEN {folder_kind}
                ELSE {separator_kind}
                END)"#,
            typ = self.typ,
            bookmark_type = BookmarkType::Bookmark as u8,
            url = self.url,
            bookmark_kind = SyncedBookmarkKind::Bookmark as u8,
            folder_type = BookmarkType::Folder as u8,
            folder_kind = SyncedBookmarkKind::Folder as u8,
            separator_kind = SyncedBookmarkKind::Separator as u8,
            query_kind = SyncedBookmarkKind::Query as u8
        )
    }
}

/// A helper that interpolates a SQL expression for a Place URL. This avoids a
/// subquery if the URL is already available in the projection.
enum UrlOrPlaceId<'a> {
    Url(&'a str),
    PlaceId(&'a str),
}

impl<'a> fmt::Display for UrlOrPlaceId<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UrlOrPlaceId::Url(s) => write!(f, "{}", s),
            UrlOrPlaceId::PlaceId(s) => {
                write!(f, "(SELECT h.url FROM moz_places h WHERE h.id = {})", s)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::places_api::{test::new_mem_api, ConnectionType};
    use crate::bookmark_sync::store::BookmarksStore;
    use crate::db::PlacesDb;
    use crate::storage::bookmarks::get_raw_bookmark;
    use crate::tests::{
        assert_json_tree as assert_local_json_tree, insert_json_tree as insert_local_json_tree,
        MirrorBookmarkItem,
    };
    use dogear::{Store as DogearStore, Validity};
    use pretty_assertions::assert_eq;
    use serde_json::{json, Value};
    use sync15::Store as SyncStore;

    use std::cell::Cell;
    use sync15::Payload;

    fn apply_incoming(records_json: Value) -> PlacesDb {
        let api = new_mem_api();
        let conn = api
            .open_connection(ConnectionType::Sync)
            .expect("should get a connection");

        // suck records into the store.
        let store = BookmarksStore {
            db: &conn,
            client_info: &Cell::new(None),
            local_time: Timestamp::now(),
            remote_time: ServerTimestamp(0.0),
        };

        let mut incoming =
            IncomingChangeset::new(store.collection_name().to_string(), ServerTimestamp(0.0));

        match records_json {
            Value::Array(records) => {
                for record in records {
                    let payload = Payload::from_json(record).unwrap();
                    incoming.changes.push((payload, ServerTimestamp(0.0)));
                }
            }
            Value::Object(_) => {
                let payload = Payload::from_json(records_json).unwrap();
                incoming.changes.push((payload, ServerTimestamp(0.0)));
            }
            _ => panic!("unexpected json value"),
        }

        store
            .apply_incoming(incoming, &mut telemetry::EngineIncoming::new())
            .expect("Should apply incoming and stage outgoing records");
        conn
    }

    fn assert_incoming_creates_local_tree(
        records_json: Value,
        local_folder: &SyncGuid,
        local_tree: Value,
    ) {
        let conn = apply_incoming(records_json);
        assert_local_json_tree(&conn, local_folder, local_tree);
    }

    fn assert_incoming_creates_mirror_item(record_json: Value, expected: &MirrorBookmarkItem) {
        let guid = record_json["id"]
            .as_str()
            .expect("id must be a string")
            .to_string();
        let conn = apply_incoming(record_json);
        let got = MirrorBookmarkItem::get(&conn, &guid.into())
            .expect("should work")
            .expect("item should exist");
        assert_eq!(*expected, got);
    }

    #[test]
    fn test_apply_tombstone() {
        assert_incoming_creates_mirror_item(
            json!({
                "id": "deadbeef____",
                "deleted": true
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .is_deleted(true),
        );
    }

    #[test]
    fn test_apply_query() {
        // First check that various inputs result in the expected records in
        // the mirror table.

        // A valid query (which actually looks just like a bookmark, but that's ok)
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "parentName": "Unfiled Bookmarks",
                "dateAdded": 1381542355843u64,
                "title": "Some query",
                "bmkUri": "place:tag=foo",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Query)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .title(Some("Some query"))
                .url(Some("place:tag=foo")),
        );

        // A query with an old "type=" param and a valid folderName. Should
        // get Reupload due to rewriting the URL.
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "bmkUri": "place:type=7",
                "folderName": "a-folder-name",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Reupload)
                .kind(SyncedBookmarkKind::Query)
                .url(Some("place:tag=a-folder-name")),
        );

        // A query with an old "type=" param and an invalid folderName. Should
        // get replaced with an empty URL
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "bmkUri": "place:type=7",
                "folderName": "",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Replace)
                .kind(SyncedBookmarkKind::Query)
                .url(None),
        );

        // A query with an old "folder=" but no excludeItems - should be
        // marked as Reupload due to the URL being rewritten.
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "bmkUri": "place:folder=123",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Reupload)
                .kind(SyncedBookmarkKind::Query)
                .url(Some("place:folder=123&excludeItems=1")),
        );

        // A query with an old "folder=" and already with  excludeItems -
        // should be marked as Valid
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "bmkUri": "place:folder=123&excludeItems=1",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Query)
                .url(Some("place:folder=123&excludeItems=1")),
        );

        // A query with a URL that can't be parsed.
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "bmkUri": "foo",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Replace)
                .kind(SyncedBookmarkKind::Query)
                .url(None),
        );

        // With a missing URL
        assert_incoming_creates_mirror_item(
            json!({
                "id": "query1______",
                "type": "query",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Replace)
                .kind(SyncedBookmarkKind::Query)
                .url(None),
        );

        // And finally, a more "functional" test - that our queries end up
        // correctly in the local tree.
        // XXX - this test fails for reasons I don't understand. If we write
        // to the mirror with kind=Bookmark, then it *does* get applied.
        // XXX - todo - add some more query variations here.
        /*
            assert_incoming_creates_local_tree(
                json!([{
                    // A valid query (which actually looks just like a bookmark, but that's ok)
                    "id": "query1______",
                    "type": "query",
                    "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                    "parentName": "Unfiled Bookmarks",
                    "dateAdded": 1381542355843u64,
                    "title": "Some query",
                    "bmkUri": "place:tag=foo",
                }]),
                &BookmarkRootGuid::Unfiled.as_guid(),
                json!({"children" : [{"guid": "query1______", "url": "place:tag=foo"}]}),
            );
        */
    }

    #[test]
    fn test_apply_sep() {
        // Separators don't have much variation.
        assert_incoming_creates_mirror_item(
            json!({
                "id": "sep1________",
                "type": "separator",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "parentName": "Unfiled Bookmarks",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Separator)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid())),
        );
    }

    #[test]
    fn test_apply_livemark() {
        // A livemark with missing URLs
        assert_incoming_creates_mirror_item(
            json!({
                "id": "livemark1___",
                "type": "livemark",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "parentName": "Unfiled Bookmarks",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Replace)
                .kind(SyncedBookmarkKind::Livemark)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .feed_url(None)
                .site_url(None),
        );
        // Valid feed_url but invalid site_url is considered "valid", but the
        // invalid URL is dropped.
        assert_incoming_creates_mirror_item(
            json!({
                "id": "livemark1___",
                "type": "livemark",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "parentName": "Unfiled Bookmarks",
                "feedUri": "http://example.com",
                "siteUri": "foo"
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Livemark)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .feed_url(Some("http://example.com/"))
                .site_url(None),
        );
        // Everything valid
        assert_incoming_creates_mirror_item(
            json!({
                "id": "livemark1___",
                "type": "livemark",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "parentName": "Unfiled Bookmarks",
                "feedUri": "http://example.com",
                "siteUri": "http://example.com/something"
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Livemark)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .feed_url(Some("http://example.com/"))
                .site_url(Some("http://example.com/something")),
        );
    }

    #[test]
    fn test_fetch_remote_tree() -> Result<()> {
        let records = vec![
            json!({
                "id": "qqVTRWhLBOu3",
                "type": "bookmark",
                "parentid": BookmarkRootGuid::Unfiled.as_guid(),
                "parentName": "Unfiled Bookmarks",
                "dateAdded": 1381542355843u64,
                "title": "The title",
                "bmkUri": "https://example.com",
                "tags": [],
            }),
            json!({
                "id": BookmarkRootGuid::Unfiled.as_guid(),
                "type": "folder",
                "parentid": BookmarkRootGuid::Root.as_guid(),
                "parentName": "",
                "dateAdded": 0,
                "title": "Unfiled Bookmarks",
                "children": ["qqVTRWhLBOu3"],
                "tags": [],
            }),
        ];

        let api = new_mem_api();
        let conn = api.open_connection(ConnectionType::Sync)?;

        // suck records into the store.
        let store = BookmarksStore {
            db: &conn,
            client_info: &Cell::new(None),
            local_time: Timestamp::now(),
            remote_time: ServerTimestamp(0.0),
        };

        for record in records {
            let payload = Payload::from_json(record).unwrap();
            store.apply_payload(ServerTimestamp(0.0), payload)?;
        }

        let tree = store.fetch_remote_tree()?;

        // should be each user root, plus the real root, plus the bookmark we added.
        assert_eq!(
            tree.guids().count(),
            BookmarkRootGuid::user_roots().len() + 2
        );

        let node = tree
            .node_for_guid(&"qqVTRWhLBOu3".into())
            .expect("should exist");
        assert_eq!(node.needs_merge, true);
        assert_eq!(node.validity, Validity::Valid);
        assert_eq!(node.level(), 2);
        assert_eq!(node.is_syncable(), true);

        let node = tree
            .node_for_guid(&BookmarkRootGuid::Unfiled.as_guid().into())
            .expect("should exist");
        assert_eq!(node.needs_merge, true);
        assert_eq!(node.validity, Validity::Valid);
        assert_eq!(node.level(), 1);
        assert_eq!(node.is_syncable(), true);

        let node = tree
            .node_for_guid(&BookmarkRootGuid::Menu.as_guid().into())
            .expect("should exist");
        assert_eq!(node.needs_merge, false);
        assert_eq!(node.validity, Validity::Valid);
        assert_eq!(node.level(), 1);
        assert_eq!(node.is_syncable(), true);

        let node = tree
            .node_for_guid(&BookmarkRootGuid::Root.as_guid().into())
            .expect("should exist");
        assert_eq!(node.needs_merge, false);
        assert_eq!(node.validity, Validity::Valid);
        assert_eq!(node.level(), 0);
        assert_eq!(node.is_syncable(), false);

        // We should have changes.
        assert_eq!(store.has_changes().unwrap(), true);
        Ok(())
    }

    #[test]
    fn test_fetch_local_tree() -> Result<()> {
        let api = new_mem_api();
        let conn = api.open_connection(ConnectionType::Sync)?;

        conn.execute("UPDATE moz_bookmarks SET syncChangeCounter = 0", NO_PARAMS)
            .expect("should work");

        insert_local_json_tree(
            &conn,
            json!({
                "guid": &BookmarkRootGuid::Unfiled.as_guid(),
                "children": [
                    {
                        "guid": "bookmark1___",
                        "title": "the bookmark",
                        "url": "https://www.example.com/"
                    },
                ]
            }),
        );

        let store = BookmarksStore {
            db: &conn,
            client_info: &Cell::new(None),
            local_time: Timestamp::now(),
            remote_time: ServerTimestamp(0.0),
        };
        let tree = store.fetch_local_tree()?;

        // should be each user root, plus the real root, plus the bookmark we added.
        assert_eq!(
            tree.guids().count(),
            BookmarkRootGuid::user_roots().len() + 2
        );

        let node = tree
            .node_for_guid(&"bookmark1___".into())
            .expect("should exist");
        assert_eq!(node.needs_merge, true);
        assert_eq!(node.level(), 2);
        assert_eq!(node.is_syncable(), true);

        let node = tree
            .node_for_guid(&BookmarkRootGuid::Unfiled.as_guid().into())
            .expect("should exist");
        assert_eq!(node.needs_merge, true);
        assert_eq!(node.level(), 1);
        assert_eq!(node.is_syncable(), true);

        let node = tree
            .node_for_guid(&BookmarkRootGuid::Menu.as_guid().into())
            .expect("should exist");
        assert_eq!(node.needs_merge, false);
        assert_eq!(node.level(), 1);
        assert_eq!(node.is_syncable(), true);

        let node = tree
            .node_for_guid(&BookmarkRootGuid::Root.as_guid().into())
            .expect("should exist");
        assert_eq!(node.needs_merge, false);
        assert_eq!(node.level(), 0);
        assert_eq!(node.is_syncable(), false);

        // We should have changes.
        assert_eq!(store.has_changes().unwrap(), true);
        Ok(())
    }

    #[test]
    fn test_apply() -> Result<()> {
        let api = new_mem_api();
        let conn = api.open_connection(ConnectionType::Sync)?;

        conn.execute("UPDATE moz_bookmarks SET syncChangeCounter = 0", NO_PARAMS)
            .expect("should work");

        insert_local_json_tree(
            &conn,
            json!({
                "guid": &BookmarkRootGuid::Unfiled.as_guid(),
                "children": [
                    {
                        "guid": "bookmarkAAAA",
                        "title": "A",
                        "url": "http://example.com/a",
                    },
                    {
                        "guid": "bookmarkBBBB",
                        "title": "B",
                        "url": "http://example.com/b",
                    },
                ]
            }),
        );

        let records = vec![
            json!({
                "id": "bookmarkCCCC",
                "type": "bookmark",
                "parentid": BookmarkRootGuid::Menu.as_guid(),
                "parentName": "menu",
                "dateAdded": 1552183116885u64,
                "title": "C",
                "bmkUri": "http://example.com/c",
                "tags": [],
            }),
            json!({
                "id": BookmarkRootGuid::Menu.as_guid(),
                "type": "folder",
                "parentid": BookmarkRootGuid::Root.as_guid(),
                "parentName": "",
                "dateAdded": 0,
                "title": "menu",
                "children": ["bookmarkCCCC"],
            }),
        ];

        let mut store = BookmarksStore {
            db: &conn,
            client_info: &Cell::new(None),
            local_time: Timestamp::now(),
            remote_time: ServerTimestamp(0.0),
        };

        let mut incoming =
            IncomingChangeset::new(store.collection_name().to_string(), ServerTimestamp(0.0));
        for record in records {
            let payload = Payload::from_json(record).unwrap();
            incoming.changes.push((payload, ServerTimestamp(0.0)));
        }

        let mut outgoing = store
            .apply_incoming(incoming, &mut telemetry::EngineIncoming::new())
            .expect("Should apply incoming and stage outgoing records");
        outgoing.changes.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            outgoing.changes.iter().map(|p| &p.id).collect::<Vec<_>>(),
            vec!["bookmarkAAAA", "bookmarkBBBB", "unfiled",]
        );

        assert_local_json_tree(
            &conn,
            &BookmarkRootGuid::Root.as_guid(),
            json!({
                "guid": &BookmarkRootGuid::Root.as_guid(),
                "children": [
                    {
                        "guid": &BookmarkRootGuid::Menu.as_guid(),
                        "children": [
                            {
                                "guid": "bookmarkCCCC",
                                "title": "C",
                                "url": "http://example.com/c",
                                "date_added": Timestamp(1552183116885),
                            },
                        ],
                    },
                    {
                        "guid": &BookmarkRootGuid::Toolbar.as_guid(),
                        "children": [],
                    },
                    {
                        "guid": &BookmarkRootGuid::Unfiled.as_guid(),
                        "children": [
                            {
                                "guid": "bookmarkAAAA",
                                "title": "A",
                                "url": "http://example.com/a",
                            },
                            {
                                "guid": "bookmarkBBBB",
                                "title": "B",
                                "url": "http://example.com/b",
                            },
                        ],
                    },
                    {
                        "guid": &BookmarkRootGuid::Mobile.as_guid(),
                        "children": [],
                    },
                ],
            }),
        );

        // We haven't finished the sync yet, so all local change counts for
        // items to upload should still be > 0.
        let guid_for_a: SyncGuid = "bookmarkAAAA".into();
        let info_for_a = get_raw_bookmark(&conn, &guid_for_a)
            .expect("Should fetch info for A")
            .unwrap();
        assert_eq!(info_for_a.sync_change_counter, 1);
        let info_for_unfiled = get_raw_bookmark(&conn, &BookmarkRootGuid::Unfiled.as_guid())
            .expect("Should fetch info for unfiled")
            .unwrap();
        assert_eq!(info_for_a.sync_change_counter, 1);

        store
            .sync_finished(
                ServerTimestamp(0.0),
                &[
                    "bookmarkAAAA".into(),
                    "bookmarkBBBB".into(),
                    "unfiled".into(),
                ],
            )
            .expect("Should push synced changes back to the store");

        let info_for_a = get_raw_bookmark(&conn, &guid_for_a)
            .expect("Should fetch info for A")
            .unwrap();
        assert_eq!(info_for_a.sync_change_counter, 0);
        let info_for_unfiled = get_raw_bookmark(&conn, &BookmarkRootGuid::Unfiled.as_guid())
            .expect("Should fetch info for unfiled")
            .unwrap();
        assert_eq!(info_for_a.sync_change_counter, 0);

        Ok(())
    }
}
