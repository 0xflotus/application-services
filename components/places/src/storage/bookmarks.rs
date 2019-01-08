/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::RowId;
use super::{fetch_page_info, new_page_info};
use std::cmp::min;
use crate::error::*;
use crate::types::{SyncGuid, SyncStatus, Timestamp, BookmarkType};
use rusqlite::{Connection, Row};
use rusqlite::types::ToSql;
use sql_support::{self, ConnExt};
use url::Url;

/// Special GUIDs associated with bookmark roots.
/// It's guaranteed that the roots will always have these guids.
#[derive(Debug, PartialEq)]
enum BookmarkRootGuids {
    Root,
    Menu,
    Toolbar,
    Unfiled,
    Mobile,
}

impl BookmarkRootGuids {
    pub fn as_guid(&self) -> SyncGuid {
        match self {
            &BookmarkRootGuids::Root => SyncGuid("root________".into()),
            &BookmarkRootGuids::Menu => SyncGuid("menu________".into()),
            &BookmarkRootGuids::Toolbar => SyncGuid("toolbar_____".into()),
            &BookmarkRootGuids::Unfiled => SyncGuid("unfiled_____".into()),
            &BookmarkRootGuids::Mobile => SyncGuid("mobile______".into()),
        }
    }

    pub fn from_guid(guid: &SyncGuid) -> Option<Self> {
        match guid.as_ref() {
            "root________" => Some(BookmarkRootGuids::Root),
            "menu________" => Some(BookmarkRootGuids::Menu),
            "toolbar_____" => Some(BookmarkRootGuids::Toolbar),
            "unfiled_____" => Some(BookmarkRootGuids::Unfiled),
            "mobile______" => Some(BookmarkRootGuids::Mobile),
            _ => None,
        }
    }
}

impl From<BookmarkRootGuids> for SyncGuid {
    fn from(item: BookmarkRootGuids) -> SyncGuid {
        item.as_guid()
    }
}

fn create_root(db: &Connection, title: &str, guid: &SyncGuid, position: u32, when: &Timestamp) -> Result<()> {
    let sql = "
        INSERT INTO moz_bookmarks
            (type, position, title, dateAdded, lastModified, guid, parent,
             syncChangeCounter, syncStatus)
        VALUES
            (:item_type, :item_position, :item_title, :date_added, :last_modified, :guid,
             IFNULL((SELECT id FROM moz_bookmarks WHERE parent = 0), 0),
             1, :sync_status)
    ";
    let params: Vec<(&str, &ToSql)> = vec![
        (":item_type", &BookmarkType::Folder),
        (":item_position", &position),
        (":item_title", &title),
        (":date_added", when),
        (":last_modified", when),
        (":guid", guid),
        (":sync_status", &SyncStatus::New),
    ];
    db.execute_named_cached(sql, &params)?;
    Ok(())
}

pub fn create_bookmark_roots(db: &Connection) -> Result<()> {
    let now = Timestamp::now();
    create_root(db, "root", &BookmarkRootGuids::Root.into(), 0, &now)?;
    create_root(db, "menu", &BookmarkRootGuids::Menu.into(), 0, &now)?;
    create_root(db, "toolbar", &BookmarkRootGuids::Toolbar.into(), 1, &now)?;
    create_root(db, "unfiled", &BookmarkRootGuids::Unfiled.into(), 2, &now)?;
    create_root(db, "mobile", &BookmarkRootGuids::Mobile.into(), 3, &now)?;
    Ok(())
}

#[derive(Debug, Copy, Clone)]
pub enum BookmarkPosition {
    Specific(u32),
    Append,
}

/// Structures which can be used to insert a bookmark, folder or separator.
#[derive(Debug, Clone)]
pub struct InsertableBookmarkItem {
    pub parent_guid: SyncGuid,
    pub position: BookmarkPosition,
    pub date_added: Option<Timestamp>,
    pub last_modified: Option<Timestamp>,
    pub guid: Option<SyncGuid>,
    pub url: Url,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InsertableBookmarkSeparator {
    pub parent_guid: SyncGuid,
    pub position: BookmarkPosition,
    pub date_added: Option<Timestamp>,
    pub last_modified: Option<Timestamp>,
    pub guid: Option<SyncGuid>,
}

#[derive(Debug, Clone)]
pub struct InsertableBookmarkFolder {
    pub parent_guid: SyncGuid,
    pub position: BookmarkPosition,
    pub date_added: Option<Timestamp>,
    pub last_modified: Option<Timestamp>,
    pub guid: Option<SyncGuid>,
    pub title: Option<String>,
}

// The type used to insert the actual item.
#[derive(Debug, Clone)]
pub enum InsertableBookmark {
    BookmarkType(InsertableBookmarkItem),
    BookmarkSeparator(InsertableBookmarkSeparator),
    BookmarkFolder(InsertableBookmarkFolder),
}

// We allow all "common" fields from the sub-types to be getters on the
// InsertableBookmark type.
macro_rules! impl_common_bookmark_getter {
    ($getter_name:ident, $T:ty) => {
        fn $getter_name(&self) -> &$T {
            match self {
                InsertableBookmark::BookmarkType(b) => &b.$getter_name,
                InsertableBookmark::BookmarkSeparator(s) => &s.$getter_name,
                InsertableBookmark::BookmarkFolder(f) => &f.$getter_name,
            }
        }
    };
}

impl InsertableBookmark {
    fn bookmark_type(&self) -> BookmarkType {
        match self {
            InsertableBookmark::BookmarkType(_) => BookmarkType::Bookmark,
            InsertableBookmark::BookmarkSeparator(_) => BookmarkType::Separator,
            InsertableBookmark::BookmarkFolder(_) => BookmarkType::Folder,
        }
    }
    impl_common_bookmark_getter!(parent_guid, SyncGuid);
    impl_common_bookmark_getter!(position, BookmarkPosition);
    impl_common_bookmark_getter!(date_added, Option<Timestamp>);
    impl_common_bookmark_getter!(last_modified, Option<Timestamp>);
    impl_common_bookmark_getter!(guid, Option<SyncGuid>);
}

pub fn insert_bookmark(db: &impl ConnExt, bm: InsertableBookmark) -> Result<SyncGuid> {
    // find the row ID of the parent.
    if BookmarkRootGuids::from_guid(&bm.parent_guid()) == Some(BookmarkRootGuids::Root) {
        return Err(InvalidPlaceInfo::InvalidGuid.into());
    }
    let parent = match get_raw_bookmark(db, &bm.parent_guid())? {
        Some(p) => p,
        None => return Err(InvalidPlaceInfo::InvalidParent.into()),
    };
    if parent.bookmark_type != BookmarkType::Folder {
        return Err(InvalidPlaceInfo::InvalidParent.into());
    }
    // Do the "position" dance.
    let index: u32 = match *bm.position() {
        BookmarkPosition::Specific(index) => {
            let actual = min(index, parent.child_count);
            // must reorder existing children.
            db.execute_named_cached(
                "UPDATE moz_bookmarks SET position = position + 1
                 WHERE parent = :parent
                 AND position >= :index",
                 &[(":parent", &parent.parent_id),
                   (":index", &actual)])?;
            actual
        },
        BookmarkPosition::Append => parent.child_count,
    };
    let fk = match bm {
        InsertableBookmark::BookmarkType(ref bm) => {
            let page_info = match fetch_page_info(db, &bm.url)? {
                Some(info) => info.page,
                None => new_page_info(db, &bm.url, None)?,
            };
            Some(page_info.row_id)
        },
        _ => None,
    };
    let sql = "INSERT INTO moz_bookmarks
              (fk, type, parent, position, title, dateAdded, lastModified,
               guid, syncStatus, syncChangeCounter) VALUES
              (:fk, :type, :parent, :position, :title, :dateAdded, :lastModified,
               :guid, :syncStatus, :syncChangeCounter)";

    let guid = bm.guid().clone().unwrap_or_else(|| SyncGuid::new());
    let date_added = bm.date_added().unwrap_or_else(|| Timestamp::now());
    let last_modified = bm.last_modified().unwrap_or_else(|| Timestamp::now());

    let bookmark_type = bm.bookmark_type();
    let params: Vec<(&str, &ToSql)> = match bm {
        InsertableBookmark::BookmarkType(ref b) => {
            vec![
                (":fk", &fk),
                (":type", &bookmark_type),
                (":parent", &parent.row_id),
                (":position", &index),
                (":title", &b.title),
                (":dateAdded", &date_added),
                (":lastModified", &last_modified),
                (":guid", &guid),
                (":syncStatus", &SyncStatus::New),
                (":syncChangeCounter", &1),
             ]
        },
        InsertableBookmark::BookmarkSeparator(ref _s) => {
            vec![
                (":type", &bookmark_type),
                (":parent", &parent.row_id),
                (":position", &index),
                ///////// - ADD THE REST!
             ]
        },
        InsertableBookmark::BookmarkFolder(ref f) => {
            vec![
                (":type", &bookmark_type),
                (":parent", &parent.row_id),
                (":title", &f.title),
                (":position", &index),
                ///////// - ADD THE REST!
             ]
        },
    };
    db.execute_named_cached(sql, &params)?;
    Ok(guid)
}

/// Support for inserting and fetching a tree. Same limitations as desktop.
/// Note that the guids are optional when inserting a tree. They will always
/// have values when fetching it.
pub struct BookmarkTreeBookmark {
    pub guid: Option<SyncGuid>,
    pub date_added: Option<Timestamp>,
    pub last_modified: Option<Timestamp>,
    pub title: Option<String>,
    pub url: Url,
}

pub struct BookmarkTreeSeparator {
    pub guid: Option<SyncGuid>,
    pub date_added: Option<Timestamp>,
    pub last_modified: Option<Timestamp>,
}

pub struct BookmarkTreeFolder {
    pub guid: Option<SyncGuid>,
    pub date_added: Option<Timestamp>,
    pub last_modified: Option<Timestamp>,
    pub title: Option<String>,
    pub children: Vec<BookmarkTreeNode>,
}

pub enum BookmarkTreeNode {
    BookmarkType(BookmarkTreeBookmark),
    BookmarkSeparator(BookmarkTreeSeparator),
    BookmarkFolder(BookmarkTreeFolder),
}

fn add_subtree_infos(db: &impl ConnExt, parent: &SyncGuid, tree: &BookmarkTreeFolder, insert_infos: &mut Vec<InsertableBookmark>) -> Result<()> {
    // TODO: track last modified?
    insert_infos.reserve(tree.children.len());
    for child in &tree.children {
        let insertable = match child {
            BookmarkTreeNode::BookmarkType(b) => {
                InsertableBookmark::BookmarkType(InsertableBookmarkItem {
                    parent_guid: parent.clone(),
                    position: BookmarkPosition::Append,
                    date_added: b.date_added,
                    last_modified: b.last_modified,
                    guid: b.guid.clone(),
                    url: b.url.clone(),
                    title: b.title.clone(),
                })
            },
            BookmarkTreeNode::BookmarkSeparator(s) => {
                InsertableBookmark::BookmarkSeparator(InsertableBookmarkSeparator {
                    parent_guid: parent.clone(),
                    position: BookmarkPosition::Append,
                    date_added: s.date_added,
                    last_modified: s.last_modified,
                    guid: s.guid.clone(),
                })
            },
            BookmarkTreeNode::BookmarkFolder(f) => {
                let parent_guid = f.guid.clone().unwrap_or_else(|| SyncGuid::new());
                add_subtree_infos(db, &parent_guid, &f, insert_infos)?;
                InsertableBookmark::BookmarkFolder(InsertableBookmarkFolder {
                    parent_guid: parent.clone(),
                    position: BookmarkPosition::Append,
                    date_added: f.date_added,
                    last_modified: f.last_modified,
                    guid: Some(parent_guid.clone()),
                    title: f.title.clone(),
                })
            },
        };
        insert_infos.push(insertable);
    }
    Ok(())
}

pub fn insert_tree(db: &impl ConnExt, parent: &SyncGuid, tree: &BookmarkTreeFolder) -> Result<()> {
    let mut insert_infos: Vec<InsertableBookmark> = Vec::new();
    add_subtree_infos(db, parent, tree, &mut insert_infos)?;
    for insertable in insert_infos {
        insert_bookmark(db, insertable)?;
    }
    Ok(())
}

#[derive(Debug)]
struct FetchedTreeRow {
    level: u32,
    id: RowId,
    guid: SyncGuid,
    parent: RowId,
    parent_guid: SyncGuid,
    node_type: u32,
    index: u32,
    title: String,
    date_added: Timestamp,
    last_modified: Timestamp,
    url: Option<String>,
}

impl FetchedTreeRow {
    pub fn from_row(row: &Row) -> Result<Self> {
        Ok(Self {
            level: row.get_checked("level")?,
            id:  row.get_checked::<_, RowId>("id")?,
            guid: SyncGuid(row.get_checked::<_, String>("guid")?),
            parent: row.get_checked::<_, RowId>("parent")?,
            parent_guid: SyncGuid(row.get_checked::<_, String>("parentGuid")?),
            node_type: row.get_checked("type")?,
            index: row.get_checked("index")?,
            title: row.get_checked("title")?,
            date_added: row.get_checked("dateAdded")?,
            last_modified: row.get_checked("lastModified")?,
            url: row.get_checked::<_, Option<String>>("url")?,
        })
    }
}

pub fn fetch_tree(db: &impl ConnExt, item_guid: &SyncGuid) ->Result<()> {
    let sql = r#"
        WITH RECURSIVE
        descendants(fk, level, type, id, guid, parent, parentGuid, position,
                    title, dateAdded, lastModified) AS (
          SELECT b1.fk, 0, b1.type, b1.id, b1.guid, b1.parent,
                 (SELECT guid FROM moz_bookmarks WHERE id = b1.parent),
                 b1.position, b1.title, b1.dateAdded, b1.lastModified
          FROM moz_bookmarks b1 WHERE b1.guid=:item_guid
          UNION ALL
          SELECT b2.fk, level + 1, b2.type, b2.id, b2.guid, b2.parent,
                 descendants.guid, b2.position, b2.title, b2.dateAdded,
                 b2.lastModified
          FROM moz_bookmarks b2
          JOIN descendants ON b2.parent = descendants.id) -- AND b2.id <> :tags_folder)
        SELECT d.level, d.id, d.guid, d.parent, d.parentGuid, d.type,
               d.position AS [index], IFNULL(d.title, "") AS title, d.dateAdded,
               d.lastModified, h.url
--               (SELECT icon_url FROM moz_icons i
--                      JOIN moz_icons_to_pages ON icon_id = i.id
--                      JOIN moz_pages_w_icons pi ON page_id = pi.id
--                      WHERE pi.page_url_hash = hash(h.url) AND pi.page_url = h.url
--                      ORDER BY width DESC LIMIT 1) AS iconuri,
--               (SELECT GROUP_CONCAT(t.title, ',')
--                FROM moz_bookmarks b2
--                JOIN moz_bookmarks t ON t.id = +b2.parent AND t.parent = :tags_folder
--                WHERE b2.fk = h.id
--               ) AS tags,
--               EXISTS (SELECT 1 FROM moz_items_annos
--                       WHERE item_id = d.id LIMIT 1) AS has_annos,
--               (SELECT a.content FROM moz_annos a
--                JOIN moz_anno_attributes n ON a.anno_attribute_id = n.id
--                WHERE place_id = h.id AND n.name = :charset_anno
--               ) AS charset
        FROM descendants d
        LEFT JOIN moz_bookmarks b3 ON b3.id = d.parent
        LEFT JOIN moz_places h ON h.id = d.fk
        ORDER BY d.level, d.parent, d.position"#;

    let mut stmt = db.conn().prepare(sql)?;

    let results = stmt.query_and_then_named(
        &[(":item_guid", item_guid)],
        FetchedTreeRow::from_row,
    )?;
    // XXX - turn this back into a BookmarkTreeFolder
    // Our query guarantees that we always visit parents ahead of their
    // children.
    for result in results {
        println!("result {:?}", result);
    }
    Ok(())
}

/// A "raw" bookmark - a representation of the row and some summary fields.
#[derive(Debug)]
struct RawBookmark {
    place_id: Option<RowId>,
    row_id: RowId,
    bookmark_type: BookmarkType,
    parent_id: RowId,
    parent_guid: SyncGuid,
    position: u32,
    title: Option<String>,
    url: Option<Url>,
    date_added: Timestamp,
    date_modified: Timestamp,
    guid: SyncGuid,
    sync_status: SyncStatus,
    sync_change_counter: u32,
    child_count: u32,
    grandparent_id: RowId,
}

impl RawBookmark {
    pub fn from_row(row: &Row) -> Result<Self> {
        let place_id = row.get_checked::<_, Option<RowId>>("fk")?;
        Ok(Self {
            row_id: row.get_checked("_id")?,
            place_id,
            bookmark_type: match BookmarkType::from_u8(row.get_checked::<_, u8>("type")?) {
                Some(t) => t,
                None => if place_id.is_some() {
                            BookmarkType::Bookmark
                        } else {
                            BookmarkType::Folder
                        }
            },
            parent_id: row.get_checked("_parentId")?,
            parent_guid: row.get_checked("parentGuid")?,
            position: row.get_checked("index")?,
            title: row.get_checked::<_, Option<String>>("title")?,
            url: match row.get_checked::<_, Option<String>>("url")? {
                Some(s) => Some(Url::parse(&s)?),
                None => None,
            },
            date_added: row.get_checked("dateAdded")?,
            date_modified: row.get_checked("lastModified")?,
            guid: SyncGuid(row.get_checked::<_, String>("guid")?),
            sync_status: SyncStatus::from_u8(row.get_checked::<_, u8>("_syncStatus")?),
            sync_change_counter: row
                .get_checked::<_, Option<u32>>("syncChangeCounter")?
                .unwrap_or_default(),
            child_count: row.get_checked("_childCount")?,
            grandparent_id: row.get_checked("_grandparentId")?,

        })
    }
}

fn get_raw_bookmark(db: &impl ConnExt, guid: &SyncGuid) -> Result<Option<RawBookmark>> {
    // sql is based on fetchBookmark() in Desktop's Bookmarks.jsm, with 'fk' added.
    Ok(db.try_query_row("
        SELECT b.guid, p.guid AS parentGuid, b.position AS 'index',
               b.dateAdded, b.lastModified, b.type, b.title AS title,
               h.url AS url, b.id AS _id, b.parent AS _parentId,
               (SELECT count(*) FROM moz_bookmarks WHERE parent = b.id) AS _childCount,
               p.parent AS _grandParentId, b.syncStatus AS _syncStatus,
               -- the columns below don't appear in the desktop query
               b.fk, b.syncChangeCounter
       FROM moz_bookmarks b
       LEFT JOIN moz_bookmarks p ON p.id = b.parent
       LEFT JOIN moz_places h ON h.id = b.fk
       WHERE b.guid = :guid",
        &[(":guid", guid)],
        RawBookmark::from_row,
        true,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::PlacesDb;

    #[test]
    fn test_insert() -> Result<()> {
        let _ = env_logger::try_init();
        let conn = PlacesDb::open_in_memory(None)?;
        let url = Url::parse("https://www.example.com")?;

        let bm = InsertableBookmark::BookmarkType(InsertableBookmarkItem {
            parent_guid: BookmarkRootGuids::Unfiled.as_guid(),
            position: BookmarkPosition::Append,
            date_added: None,
            last_modified: None,
            guid: None,
            url: url.clone(),
            title: Some("the title".into()),
        });
        let guid = insert_bookmark(&conn, bm)?;

        // re-fetch it.
        let rb = get_raw_bookmark(&conn, &guid)?.expect("should get the bookmark");

        assert!(rb.place_id.is_some());
        assert_eq!(rb.bookmark_type, BookmarkType::Bookmark);
        assert_eq!(rb.parent_guid, BookmarkRootGuids::Unfiled.as_guid());
        assert_eq!(rb.position, 0);
        assert_eq!(rb.title, Some("the title".into()));
        assert_eq!(rb.url, Some(url));
        assert_eq!(rb.sync_status, SyncStatus::New);
        assert_eq!(rb.sync_change_counter, 1);
        assert_eq!(rb.child_count, 0);
        Ok(())
    }

    #[test]
    fn test_insert_pos_too_large() -> Result<()> {
        let _ = env_logger::try_init();
        let conn = PlacesDb::open_in_memory(None)?;
        let url = Url::parse("https://www.example.com")?;

        let bm = InsertableBookmark::BookmarkType(InsertableBookmarkItem {
            parent_guid: BookmarkRootGuids::Unfiled.as_guid(),
            position: BookmarkPosition::Specific(100),
            date_added: None,
            last_modified: None,
            guid: None,
            url: url.clone(),
            title: Some("the title".into()),
        });
        let guid = insert_bookmark(&conn, bm)?;

        // re-fetch it.
        let rb = get_raw_bookmark(&conn, &guid)?.expect("should get the bookmark");

        assert_eq!(rb.position, 0, "large value should have been ignored");
        Ok(())
    }

    #[test]
    fn test_insert_tree() -> Result<()> {
        let _ = env_logger::try_init();
        let conn = PlacesDb::open_in_memory(None)?;

        let tree = BookmarkTreeFolder {
            guid: None,
            date_added: None,
            last_modified: None,
            title: None,
            children: vec![
                BookmarkTreeNode::BookmarkType(BookmarkTreeBookmark {
                    guid: None,
                    date_added: None,
                    last_modified: None,
                    title: Some("the bookmark".into()),
                    url: Url::parse("https://www.example.com")?,
                })
            ],
        };
        insert_tree(&conn, &BookmarkRootGuids::Unfiled.into(), &tree)?;

        // re-fetch it.
        fetch_tree(&conn, &BookmarkRootGuids::Unfiled.into())?;

        // let rb = get_raw_bookmark(&conn, &guid)?.expect("should get the bookmark");
        Ok(())
    }


}
