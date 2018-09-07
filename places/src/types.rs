/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{fmt};
use std::time::{SystemTime, UNIX_EPOCH};
use url::{Url};

// XXX - copied from logins - should be in a shared crate
#[derive(PartialEq, Eq, Hash, Clone, Debug, Serialize, Deserialize)]
pub struct SyncGuid(pub String);

impl AsRef<str> for SyncGuid {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl<T> From<T> for SyncGuid where T: Into<String> {
    fn from(x: T) -> SyncGuid {
        SyncGuid(x.into())
    }
}

// Typesafe way to manage timestamps.
// We should probably work out how to share this too?
//
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Deserialize, Serialize, Default)]
pub struct Timestamp(pub u64);

impl From<Timestamp> for u64 {
    #[inline]
    fn from(ts: Timestamp) -> Self { ts.0 }
}

impl From<SystemTime> for Timestamp {
    #[inline]
    fn from(st: SystemTime) -> Self {
        let d = st.duration_since(UNIX_EPOCH).unwrap(); // hrmph - unwrap doesn't seem ideal
        Timestamp((d.as_secs() as u64) * 1000 + ((d.subsec_nanos() as u64) / 1_000_000))
    }
}

impl From<u64> for Timestamp {
    #[inline]
    fn from(ts: u64) -> Self {
        assert!(ts != 0);
        Timestamp(ts)
    }
}

impl fmt::Display for Timestamp {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// NOTE: These discriminator values are the same as those used by Desktop
// Firefox and are what is written to the database.
#[derive(Debug, Copy, Clone, PartialEq)]
enum VisitTransition {
    // This transition type means the user followed a link.
    Link = 1,

    // This transition type means that the user typed the page's URL in the
    // URL bar or selected it from UI (URL bar autocomplete results, etc)
    Typed = 2,

    // XXX - moar comments.
    Bookmark = 3,
    Embed = 4,
    RedirectPermanent = 5,
    RedirectTemporary = 6,
    Download = 7,
    FramedLink = 8,
    Reload = 9,
}

// Until std::num::FromPrimitive exists use this.
// (shame we can't use the From trait here!)
fn visit_from_primitive(p: u32) -> Option<VisitTransition> {
    match p {
        1 => Some(VisitTransition::Link),
        2 => Some(VisitTransition::Typed),
        3 => Some(VisitTransition::Bookmark),
        4 => Some(VisitTransition::Embed),
        5 => Some(VisitTransition::RedirectPermanent),
        6 => Some(VisitTransition::RedirectTemporary),
        7 => Some(VisitTransition::Download),
        8 => Some(VisitTransition::FramedLink),
        9 => Some(VisitTransition::Reload),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_primitive() {
        assert_eq!(Some(VisitTransition::Link), visit_from_primitive(1));
        assert_eq!(None, visit_from_primitive(99));
    }
}

#[derive(Debug)]
struct AddableVisit {
    date: Timestamp,
    transition: VisitTransition,
    referrer: Option<Url>,
}

// A struct representing a "place info" which can be added/updated.
// Consumers who want to record a visit need only supply one of these.
// (Not clear this makes sense - it's a copy of what desktop does just to
// get started)
#[derive(Debug)]
pub struct AddablePlaceInfo {
    guid: Option<SyncGuid>,
    url: Option<Url>,
    title: Option<String>,
    visits: Vec<AddableVisit>,
}
