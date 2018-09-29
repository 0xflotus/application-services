/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// A "storage" module - this module is intended to be the layer between the
// API and the database.
// This should probably be a sub-directory

use std::{fmt, cmp};
use url::{Url};
use types::{SyncGuid, Timestamp, VisitTransition};
use error::{Result};
use observation::{VisitObservation};
use frecency;

use rusqlite::{Row, Connection};
use rusqlite::{types::{ToSql, FromSql, ToSqlOutput, FromSqlResult, ValueRef}};
use rusqlite::Result as RusqliteResult;

use db::{PlacesDb, ConnectionUtil};

// Typesafe way to manage RowIds. Does it make sense? A better way?
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Deserialize, Serialize, Default)]
pub struct RowId(pub i64);

impl From<RowId> for i64 { // XXX - ToSql!
    #[inline]
    fn from(id: RowId) -> Self { id.0 }
}

impl fmt::Display for RowId {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ToSql for RowId {
    fn to_sql(&self) -> RusqliteResult<ToSqlOutput> {
        Ok(ToSqlOutput::from(self.0))
    }
}

impl FromSql for RowId {
    fn column_result(value: ValueRef) -> FromSqlResult<Self> {
        value.as_i64().map(|v| RowId(v))
    }
}

// A PageId is an enum, either a Guid or a Url, to identify a unique key for a
// page as specifying both doesn't make sense (and specifying the guid is
// slighly faster, plus sync might *only* have the guid?)
// XXX - not clear this makes sense as it doesn't make sense to create a
// moz_places row given just a guid - so, in that sense at least, it's not
// really true that you can have one or the other.
#[derive(Debug, Clone)]
pub enum PageId {
    Guid(SyncGuid),
    Url(Url),
}

#[derive(Debug)]
pub struct PageInfo {
    pub page_id: PageId,
    pub row_id: RowId,
    pub url: Url,
    pub title: String,
    pub hidden: bool,
    pub typed: u32,
    pub frecency: i32,
    pub visit_count_local: i32,
    pub visit_count_remote: i32,
    pub last_visit_date_local: Timestamp,
    pub last_visit_date_remote: Timestamp,
}

impl PageInfo {
    pub fn from_row(row: &Row) -> Result<Self> {
        Ok(Self {
            page_id: PageId::Guid(row.get_checked("guid")?),
            url: Url::parse(&row.get_checked::<_, Option<String>>("url")?.expect("non null column"))?,
            row_id: row.get_checked("id")?,
            title: row.get_checked::<_, Option<String>>("title")?.unwrap_or_default(),
            hidden: row.get_checked("hidden")?,
            typed: row.get_checked("typed")?,

            frecency:   row.get_checked("frecency")?,
            visit_count_local: row.get_checked("visit_count_local")?,
            visit_count_remote: row.get_checked("visit_count_remote")?,

            last_visit_date_local: row.get_checked::<_, Option<Timestamp>>(
                "last_visit_date_local")?.unwrap_or_default(),
            last_visit_date_remote: row.get_checked::<_, Option<Timestamp>>(
                "last_visit_date_remote")?.unwrap_or_default(),
        })
    }
}

// fetch_page_info gives you one of these.
#[derive(Debug)]
struct FetchedPageInfo {
    page: PageInfo,
    // XXX - not clear what this is used for yet, and whether it should be local, remote or either?
    // The sql below isn't quite sure either :)
    last_visit_id: RowId,
}

impl FetchedPageInfo {
    pub fn from_row(row: &Row) -> Result<Self> {
        Ok(Self {
            page: PageInfo::from_row(row)?,
            last_visit_id: row.get_checked::<_, Option<RowId>>("last_visit_id")?.expect("No visit id!"),
        })
    }
}

// History::FetchPageInfo
fn fetch_page_info(db: &impl ConnectionUtil, page_id: &PageId) -> Result<Option<FetchedPageInfo>> {
    match page_id {
        // XXX - there's way too much sql and db.query duplicated here!?
        PageId::Guid(ref guid) => {
            let sql = "
              SELECT guid, url, id, title, hidden, typed, frecency,
                     visit_count_local, visit_count_remote,
                     last_visit_date_local, last_visit_date_remote,
              (SELECT id FROM moz_historyvisits
               WHERE place_id = h.id
                 AND (visit_date = h.last_visit_date_local OR
                      visit_date = h.last_visit_date_remote)) AS last_visit_id
              FROM moz_places h
              WHERE guid = :guid";
            Ok(db.query_row_named(sql, &[(":guid", guid)], FetchedPageInfo::from_row)?)
        },
        PageId::Url(url) => {
            let sql = "
              SELECT guid, url, id, title, hidden, typed, frecency,
                     visit_count_local, visit_count_remote,
                     last_visit_date_local, last_visit_date_remote,
              (SELECT id FROM moz_historyvisits
               WHERE place_id = h.id
                 AND (visit_date = h.last_visit_date_local OR
                      visit_date = h.last_visit_date_remote)) AS last_visit_id
              FROM moz_places h
              WHERE url_hash = hash(:page_url) AND url = :page_url";
            Ok(db.query_row_named(sql, &[(":page_url", &url.clone().into_string())], FetchedPageInfo::from_row)?)
        }
    }
}

pub struct ManyVisitObservations<'conn> {
    conn: &'conn mut PlacesDb,
    pending_observations: Vec<VisitObservation>,
    limit: Option<usize>,
}

impl<'conn> ManyVisitObservations<'conn> {
    pub fn new(conn: &'conn mut PlacesDb) -> ManyVisitObservations<'conn> {
        ManyVisitObservations { conn, pending_observations: Vec::new(), limit: None }
    }

    pub fn with_limit(conn: &'conn mut PlacesDb, limit: usize) -> ManyVisitObservations<'conn> {
        ManyVisitObservations { conn, pending_observations: Vec::new(), limit: Some(limit) }
    }

    pub fn add(&mut self, visits: Vec<VisitObservation>) -> Result<()> {
        if let Some(limit) = self.limit {
            self.pending_observations.extend(visits);
            if self.pending_observations.len() < limit {
                return Ok(())
            }
            return apply_observations(&mut self.conn, &self.pending_observations[..limit]);
        }
        apply_observations(&mut self.conn, &self.pending_observations)
    }

    pub fn flush(mut self) -> Result<()> {
        apply_observations(&mut self.conn, &self.pending_observations)
    }
}

/// Inserts many new visits into the database. This should be used by Sync,
/// profile migration,
pub fn apply_observations<'v>(db: &mut PlacesDb, visit_obs: &'v [VisitObservation]) -> Result<()> {
    for observations in visit_obs.chunks(999 / 2) {
        let values = observations.iter().map(|visit|
            format!("(?, {has_title}, ?, {visit_type}, {hidden}, {was_typed}, {is_local}, {last_visit_date})",
                has_title=visit.get_title().map_or(0, |_| 1),
                visit_type=visit.get_visit_type().map_or("NULL".to_string(), |typ| (typ as u8).to_string()),
                hidden=visit.get_is_hidden() as u8,
                was_typed=visit.get_was_typed() as u8,
                is_local=!(visit.get_is_remote()) as u8,
                last_visit_date=visit.get_at().map_or("NULL".to_string(), |at| at.to_string()),
            )
        ).collect::<Vec<String>>().join(",");
        let query = format!("
            INSERT INTO observations_to_apply(url, has_title, title, visit_type, hidden,
                                              was_typed, is_local, last_visit_date)
            VALUES {}
        ", values);
        let mut params: Vec<Option<String>> = Vec::with_capacity(observations.len() * 2);
        for visit in observations {
            params.push(visit.get_url().map(|url| url.as_str().to_owned()));
            params.push(visit.get_title().map(|title| title.as_str().to_owned()));
        }
        // TODO: This isn't supported yet; see
        // https://github.com/jgallagher/rusqlite/issues/312
        // db.db.execute(&query, &params)?;
    }
    Ok(())
}

pub fn apply_observation(db: &mut PlacesDb, visit_ob: VisitObservation) -> Result<()> {
    let tx = db.db.transaction()?;
    apply_observation_direct(tx.conn(), visit_ob)?;
    tx.commit()?;
    Ok(())
}

pub fn apply_observation_direct(db: &Connection, visit_ob: VisitObservation) -> Result<()> {
    let mut page_info = match fetch_page_info(db, &visit_ob.page_id)? {
        Some(info) => info.page,
        None => new_page_info(db, &visit_ob.page_id)?,
    };
    let mut updates: Vec<(&str, &str, &ToSql)> = Vec::new();
    if let Some(title) = visit_ob.get_title() {
        page_info.title = title.clone();
        updates.push(("title", ":title", &page_info.title));
    }

    let mut update_frecency = false;

    // There's a new visit, so update everything that implies
    if let Some(visit_type) = visit_ob.get_visit_type() {
        // A single non-hidden visit makes the place non-hidden.
        if !visit_ob.get_is_hidden() {
            updates.push(("hidden", ":hidden", &false));
        }
        if visit_ob.get_was_typed() {
            page_info.typed += 1;
            updates.push(("typed", ":typed", &page_info.typed));
        }

        let at = visit_ob.get_at().unwrap_or_else(|| Timestamp::now());
        let is_remote = visit_ob.get_is_remote();
        add_visit(db, &page_info.row_id, &None, &at, &visit_type, &is_remote)?;
        if is_remote {
            page_info.visit_count_remote += 1;
            updates.push(("visit_count_remote", ":visit_count_remote", &page_info.visit_count_remote));
            page_info.last_visit_date_remote = cmp::max(at, page_info.last_visit_date_remote);
            updates.push(("last_visit_date_remote", ":last_visit_date_remote", &page_info.last_visit_date_remote));
        } else {
            page_info.visit_count_local += 1;
            updates.push(("visit_count_local", ":visit_count_local", &page_info.visit_count_local));
            page_info.last_visit_date_local = cmp::max(at, page_info.last_visit_date_local);
            updates.push(("last_visit_date_local", ":last_visit_date_local", &page_info.last_visit_date_local));
        }
        // a new visit implies new frecency except in error cases.
        if !visit_ob.get_is_error() {
            update_frecency = true;
        }
    }
    if updates.len() != 0 {
        let mut params: Vec<(&str, &ToSql)> = Vec::with_capacity(updates.len() + 1);
        let mut sets: Vec<String> = Vec::with_capacity(updates.len());
        for (col, name, val) in updates {
            sets.push(format!("{} = {}", col, name));
            params.push((name, val))
        }
        params.push((":row_id", &page_info.row_id.0));
        let sql = format!("UPDATE moz_places
                          SET {}
                          WHERE id == :row_id", sets.join(","));
        db.execute_named_cached(&sql, &params)?;
    }
    // This needs to happen after the other updates.
    if update_frecency {
        page_info.frecency = frecency::calculate_frecency(db,
            &frecency::DEFAULT_FRECENCY_SETTINGS,
            page_info.row_id.0, // TODO: calculate_frecency should take a RowId here.
            Some(visit_ob.get_redirect_frecency_boost()))?;
        let sql = "
            UPDATE moz_places
            SET frecency = :frecency
            WHERE id = :row_id
        ";
        db.execute_named_cached(sql, &[
            (":row_id", &page_info.row_id.0),
            (":frecency", &page_info.frecency),
        ])?;
    }
    Ok(())
}

fn new_page_info(db: &impl ConnectionUtil, pid: &PageId) -> Result<PageInfo> {
    match pid {
        PageId::Guid(_) => panic!("Need to think this through, but items must be created with a url"),
        PageId::Url(ref url) => {
            let guid = super::sync::util::random_guid().expect("according to logins-sql, this is fine :)");
            let sql = "INSERT INTO moz_places (guid, url, url_hash)
                       VALUES (:guid, :url, hash(:url))";
            db.execute_named_cached(sql, &[
                (":guid", &guid),
                (":url", &url.clone().into_string()),
            ])?;
            Ok(PageInfo {
                page_id: PageId::Guid(SyncGuid(guid)),
                row_id: RowId(db.conn().last_insert_rowid()),
                url: url.clone(),
                title: "".into(),
                hidden: true, // will be set to false as soon as a non-hidden visit appears.
                typed: 0,
                frecency: -1,
                visit_count_local: 0,
                visit_count_remote: 0,
                last_visit_date_local: Timestamp(0),
                last_visit_date_remote: Timestamp(0),
            })
        }
    }
}

// Add a single visit - you must know the page rowid. Does not update the
// page info - if you are calling this, you will also need to update the
// parent page with the new visit count, frecency, etc.
fn add_visit(db: &impl ConnectionUtil,
             page_id: &RowId,
             from_visit: &Option<RowId>,
             visit_date: &Timestamp,
             visit_type: &VisitTransition,
             is_local: &bool) -> Result<RowId> {
    let sql =
        "INSERT INTO moz_historyvisits
            (from_visit, place_id, visit_date, visit_type, is_local)
        VALUES (:from_visit, :page_id, :visit_date, :visit_type, :is_local)";
    db.execute_named_cached(sql, &[
        (":from_visit", from_visit),
        (":page_id", page_id),
        (":visit_date", visit_date),
        (":visit_type", visit_type),
        (":is_local", is_local),
    ])?;
    let rid = db.conn().last_insert_rowid();
    Ok(RowId(rid))
}

// Currently not used - we update the frecency as we update the page info.
pub fn update_frecency(db: &mut PlacesDb, id: RowId, redirect: Option<bool>) -> Result<()> {
    let score = frecency::calculate_frecency(db.conn(),
        &frecency::DEFAULT_FRECENCY_SETTINGS,
        id.0, // TODO: calculate_frecency should take a RowId here.
        redirect)?;

    db.execute_named("
        UPDATE moz_places
        SET frecency = :frecency
        WHERE id = :page_id",
        &[(":frecency", &score), (":page_id", &id.0)])?;

    Ok(())
}

// Mini experiment with an "Origin" object that knows how to rev_host() itself,
// that I don't want to throw away yet :) I'm really not sure exactly how
// moz_origins fits in TBH :/
#[cfg(test)]
mod tests {
    use unicode_segmentation::UnicodeSegmentation;

    struct Origin {
        prefix: String,
        host: String,
        frecency: i64,
    }
    impl Origin {
        pub fn rev_host(&self) -> String {
            self.host.graphemes(true).rev().flat_map(|g| g.chars()).collect()
        }
    }

    #[test]
    fn test_reverse() {
        let o = Origin {prefix: "http".to_string(),
                        host: "foo.com".to_string(),
                        frecency: 0 };
        assert_eq!(o.prefix, "http");
        assert_eq!(o.frecency, 0);
        assert_eq!(o.rev_host(), "moc.oof");
    }

}
