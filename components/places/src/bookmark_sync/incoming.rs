/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::record::{
    BookmarkItemRecord, BookmarkRecord, FolderRecord, LivemarkRecord, QueryRecord, SeparatorRecord,
};
use super::{SyncedBookmarkKind, SyncedBookmarkValidity};
use crate::error::*;
use crate::storage::{bookmarks::maybe_truncate_title, TAG_LENGTH_MAX, URL_LENGTH_MAX};
use crate::types::SyncGuid;
use rusqlite::Connection;
use sql_support::{self, ConnExt};
use sync15::ServerTimestamp;
use url::Url;

// From Desktop's Ci.nsINavHistoryQueryOptions, but we define it as a str
// as that's how we use it here.
const RESULTS_AS_TAG_CONTENTS: &str = "7";

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

/// Manages the application of incoming records into the moz_bookmarks_synced
/// and related tables.
pub struct IncomingApplicator<'a> {
    db: &'a Connection,
}

impl<'a> IncomingApplicator<'a> {
    pub fn new(db: &'a Connection) -> Self {
        Self { db }
    }

    pub fn apply_payload(
        &self,
        payload: sync15::Payload,
        timestamp: ServerTimestamp,
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
                log::warn!("query {} has invalid URL '{:?}': {:?}", q.guid, q.url, e);
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
                    }
                    Err(e) => {
                        log::warn!(
                            "Livemark {} has an invalid {} URL '{}': {:?}",
                            &guid,
                            what,
                            h,
                            e
                        );
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::places_api::{test::new_mem_api, ConnectionType};
    use crate::db::PlacesDb;
    use crate::storage::bookmarks::BookmarkRootGuid;

    use crate::bookmark_sync::tests::MirrorBookmarkItem;
    use pretty_assertions::assert_eq;
    use serde_json::{json, Value};
    use sync15::Payload;

    fn apply_incoming(records_json: Value) -> PlacesDb {
        let api = new_mem_api();
        let conn = api
            .open_connection(ConnectionType::Sync)
            .expect("should get a connection");

        let server_timestamp = ServerTimestamp(0.0);
        let applicator = IncomingApplicator::new(&conn);

        match records_json {
            Value::Array(records) => {
                for record in records {
                    let payload = Payload::from_json(record).unwrap();
                    applicator
                        .apply_payload(payload, server_timestamp)
                        .expect("Should apply incoming and stage outgoing records");
                }
            }
            Value::Object(_) => {
                let payload = Payload::from_json(records_json).unwrap();
                applicator
                    .apply_payload(payload, server_timestamp)
                    .expect("Should apply incoming and stage outgoing records");
            }
            _ => panic!("unexpected json value"),
        }

        conn
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
                "parentid": "unfiled",
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
                "parentid": "unfiled",
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
                "parentid": "unfiled",
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
                "parentid": "unfiled",
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
                "parentid": "unfiled",
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
                "parentid": "unfiled",
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
                "parentid": "unfiled",
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
                    "parentid": "unfiled",
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
                "parentid": "unfiled",
                "parentName": "Unfiled Bookmarks",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Separator)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .needs_merge(true),
        );
    }

    #[test]
    fn test_apply_livemark() {
        // A livemark with missing URLs
        assert_incoming_creates_mirror_item(
            json!({
                "id": "livemark1___",
                "type": "livemark",
                "parentid": "unfiled",
                "parentName": "Unfiled Bookmarks",
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Replace)
                .kind(SyncedBookmarkKind::Livemark)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .needs_merge(true)
                .feed_url(None)
                .site_url(None),
        );
        // Valid feed_url but invalid site_url is considered "valid", but the
        // invalid URL is dropped.
        assert_incoming_creates_mirror_item(
            json!({
                "id": "livemark1___",
                "type": "livemark",
                "parentid": "unfiled",
                "parentName": "Unfiled Bookmarks",
                "feedUri": "http://example.com",
                "siteUri": "foo"
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Livemark)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .needs_merge(true)
                .feed_url(Some("http://example.com/"))
                .site_url(None),
        );
        // Everything valid
        assert_incoming_creates_mirror_item(
            json!({
                "id": "livemark1___",
                "type": "livemark",
                "parentid": "unfiled",
                "parentName": "Unfiled Bookmarks",
                "feedUri": "http://example.com",
                "siteUri": "http://example.com/something"
            }),
            &MirrorBookmarkItem::new()
                .validity(SyncedBookmarkValidity::Valid)
                .kind(SyncedBookmarkKind::Livemark)
                .parent_guid(Some(&BookmarkRootGuid::Unfiled.as_guid()))
                .needs_merge(true)
                .feed_url(Some("http://example.com/"))
                .site_url(Some("http://example.com/something")),
        );
    }
}
