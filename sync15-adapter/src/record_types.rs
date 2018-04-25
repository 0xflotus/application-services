/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// use error::{ErrorKind, Result};
use bso_record::{BsoRecord, Sync15Record};

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum MaybeTombstone<T> {
    Tombstone { id: String, deleted: bool },
    Record(T)
}

impl<T> MaybeTombstone<T> {
    #[inline]
    pub fn tombstone<R: Into<String>>(id: R) -> MaybeTombstone<T> {
        MaybeTombstone::Tombstone { id: id.into(), deleted: true }
    }

    #[inline]
    pub fn is_tombstone(&self) -> bool {
        match self {
            &MaybeTombstone::Record(_) => false,
            _ => true
        }
    }

    #[inline]
    pub fn unwrap(self) -> T {
        match self {
            MaybeTombstone::Record(record) => record,
            _ => panic!("called `MaybeTombstone::unwrap()` on a Tombstone!"),
        }
    }

    #[inline]
    pub fn expect(self, msg: &str) -> T {
        match self {
            MaybeTombstone::Record(record) => record,
            _ => panic!("{}", msg),
        }
    }

    #[inline]
    pub fn ok_or<E>(self, err: E) -> ::std::result::Result<T, E> {
        match self {
            MaybeTombstone::Record(record) => Ok(record),
            _ => Err(err)
        }
    }

    #[inline]
    pub fn record(self) -> Option<T> {
        match self {
            MaybeTombstone::Record(record) => Some(record),
            _ => None
        }
    }
}

impl<T> Sync15Record for MaybeTombstone<T> where T: Sync15Record {
    fn collection_tag() -> &'static str { T::collection_tag() }
    fn record_id(&self) -> &str {
        match self {
            &MaybeTombstone::Tombstone { ref id, .. } => id,
            &MaybeTombstone::Record(ref record) => record.record_id()
        }
    }
}

impl<T> BsoRecord<MaybeTombstone<T>> {
    #[inline]
    pub fn is_tombstone(&self) -> bool {
        self.payload.is_tombstone()
    }

    #[inline]
    pub fn record(self) -> Option<BsoRecord<T>> where T: Clone {
        self.map_payload(|payload| payload.record()).transpose()
    }
}

pub type MaybeTombstoneRecord<T> = BsoRecord<MaybeTombstone<T>>;

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordRecord {
    pub id: String,
    pub hostname: Option<String>,

    // rename_all = "camelCase" by default will do formSubmitUrl, but we can just
    // override this one field.
    #[serde(rename = "formSubmitURL")]
    pub form_submit_url: Option<String>,

    pub http_realm: Option<String>,

    #[serde(default = "String::new")]
    pub username: String,

    pub password: String,

    #[serde(default = "String::new")]
    pub username_field: String,

    #[serde(default = "String::new")]
    pub password_field: String,

    pub time_created: i64,
    pub time_password_changed: i64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_last_used: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub times_used: Option<i64>,
}

impl Sync15Record for PasswordRecord {
    fn collection_tag() -> &'static str { "passwords" }
    fn record_id(&self) -> &str { &self.id }
}

#[cfg(test)]
mod tests {

    use super::*;
    use key_bundle::KeyBundle;

    #[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
    struct DummyRecord {
        id: String,
        age: i64,
        meta: String,
    }

    impl Sync15Record for DummyRecord {
        fn collection_tag() -> &'static str { "dummy" }
        fn record_id(&self) -> &str { &self.id }
    }

    #[test]
    fn test_roundtrip_crypt_tombstone() {
        let orig_record: MaybeTombstoneRecord<DummyRecord> = BsoRecord {
            id: "aaaaaaaaaaaa".into(),
            collection: "dummy".into(),
            modified: 1234.0,
            sortindex: None,
            ttl: None,
            payload: MaybeTombstone::tombstone("aaaaaaaaaaaa")
        };

        assert!(orig_record.is_tombstone());

        let keybundle = KeyBundle::new_random().unwrap();

        let encrypted = orig_record.clone().encrypt(&keybundle).unwrap();

        assert!(keybundle.verify_hmac_string(
            &encrypted.payload.hmac, &encrypted.payload.ciphertext).unwrap());

        let decrypted: MaybeTombstoneRecord<DummyRecord> = encrypted.decrypt(&keybundle).unwrap();
        assert!(decrypted.is_tombstone());
        assert_eq!(decrypted, orig_record);
    }

    #[test]
    fn test_roundtrip_crypt_record() {
        let orig_record: MaybeTombstoneRecord<DummyRecord> = BsoRecord {
            id: "aaaaaaaaaaaa".into(),
            collection: "dummy".into(),
            modified: 1234.0,
            sortindex: None,
            ttl: None,
            payload: MaybeTombstone::Record(DummyRecord {
                id: "aaaaaaaaaaaa".into(),
                age: 105,
                meta: "data".into()
            })
        };

        assert!(!orig_record.is_tombstone());

        let keybundle = KeyBundle::new_random().unwrap();

        let encrypted = orig_record.clone().encrypt(&keybundle).unwrap();

        assert!(keybundle.verify_hmac_string(
            &encrypted.payload.hmac, &encrypted.payload.ciphertext).unwrap());

        let decrypted: MaybeTombstoneRecord<DummyRecord> = encrypted.decrypt(&keybundle).unwrap();
        assert!(!decrypted.is_tombstone());
        assert_eq!(decrypted, orig_record);
    }
}
