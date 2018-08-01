/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::collections::HashMap;
use std::time::Duration;

use hyper::{Method, StatusCode};
use reqwest::{Client, Request, Response, Url, header::{self, Accept}};
use serde;

use bso_record::{BsoRecord, EncryptedBso};
use error::{self, ErrorKind};
use record_types::MetaGlobalRecord;
use request::{BatchPoster, CollectionRequest, InfoConfiguration, PostQueue, PostResponse,
              PostResponseHandler, XIfUnmodifiedSince, XWeaveTimestamp};
use token;
use util::ServerTimestamp;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sync15StorageClientInit {
    pub key_id: String,
    pub access_token: String,
    pub tokenserver_base_url: String,
}

#[derive(Debug)]
pub struct Sync15StorageClient {
    http_client: Client,
    // We update this when we make requests
    timestamp: Cell<ServerTimestamp>,
    tsc: token::TokenProvider,
}

impl Sync15StorageClient {
    pub fn new(init_params: Sync15StorageClientInit) -> error::Result<Sync15StorageClient> {
        let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
        let tsc = token::TokenProvider::new(
            init_params.tokenserver_base_url.clone(),
            init_params.access_token.clone(),
            init_params.key_id.clone(),
        );
        let timestamp = ServerTimestamp(0f64);
        Ok(Sync15StorageClient {
            http_client: client,
            timestamp: Cell::new(timestamp),
            tsc,
        })
    }

    #[inline]
    pub fn last_server_time(&self) -> ServerTimestamp {
        return self.timestamp.get();
    }

    pub fn fetch_info_configuration(&self) -> error::Result<InfoConfiguration> {
        let server_config = self.fetch_info::<InfoConfiguration>("info/configuration")?;
        Ok(server_config)
    }

    pub fn fetch_info_collections(&self) -> error::Result<HashMap<String, ServerTimestamp>> {
        let collections = self.fetch_info::<HashMap<String, ServerTimestamp>>("info/collections")?;
        Ok(collections)
    }

    pub fn fetch_meta_global(&self) -> error::Result<BsoRecord<MetaGlobalRecord>> {
        let mut resp = match self.relative_storage_request(Method::Get, "storage/meta/global") {
            Ok(r) => r,
            // This is gross, but at least it works. Replace 404s on meta/global with NoMetaGlobal.
            Err(e) => {
                if let ErrorKind::StorageHttpError {
                    code: StatusCode::NotFound,
                    ..
                } = e.kind()
                {
                    return Err(ErrorKind::NoMetaGlobal.into());
                }
                return Err(e);
            }
        };
        // Note: meta/global is not encrypted!
        let meta_global: BsoRecord<MetaGlobalRecord> = resp.json()?;
        info!("Meta global: {:?}", meta_global.payload);
        Ok(meta_global)
    }

    pub fn fetch_crypto_keys(&self) -> error::Result<EncryptedBso> {
        let mut keys_resp = self.relative_storage_request(Method::Get, "storage/crypto/keys")?;
        let keys: EncryptedBso = keys_resp.json()?;
        Ok(keys)
    }

    pub fn get_encrypted_records(
        &self,
        collection: &str,
        since: ServerTimestamp,
    ) -> error::Result<Vec<EncryptedBso>> {
        let mut resp = self.collection_request(
            Method::Get,
            CollectionRequest::new(collection).full().newer_than(since),
        )?;
        Ok(resp.json()?)
    }

    #[inline]
    fn authorized(&self, mut req: Request) -> error::Result<Request> {
        let header = self.tsc.authorization(&self.http_client, &req)?;
        req.headers_mut().set(header);
        Ok(req)
    }

    // TODO: probably want a builder-like API to do collection requests (e.g. something
    // that occupies roughly the same conceptual role as the Collection class in desktop)
    fn build_request(&self, method: Method, url: Url) -> error::Result<Request> {
        self.authorized(self.http_client
            .request(method, url)
            .header(Accept::json())
            .build()?)
    }

    fn relative_storage_request<T>(
        &self,
        method: Method,
        relative_path: T,
    ) -> error::Result<Response>
    where
        T: AsRef<str>,
    {
        let s = self.tsc.api_endpoint(&self.http_client)? + "/";
        let url = Url::parse(&s)?.join(relative_path.as_ref())?;
        Ok(self.make_storage_request(method, url)?)
    }

    fn make_storage_request(&self, method: Method, url: Url) -> error::Result<Response> {
        // I'm shocked that method isn't Copy...
        Ok(self.exec_request(self.build_request(method.clone(), url)?, true)?)
    }

    fn exec_request(&self, req: Request, require_success: bool) -> error::Result<Response> {
        let resp = self.http_client.execute(req)?;

        self.update_timestamp(resp.headers());

        if require_success && !resp.status().is_success() {
            error!(
                "HTTP error {} ({}) during storage request to {}",
                resp.status().as_u16(),
                resp.status(),
                resp.url().path()
            );
            return Err(ErrorKind::StorageHttpError {
                code: resp.status(),
                route: resp.url().path().into(),
            }.into());
        }

        // TODO:
        // - handle backoff
        // - x-weave-quota?
        // - ... almost certainly other things too...

        Ok(resp)
    }

    fn collection_request(&self, method: Method, r: &CollectionRequest) -> error::Result<Response> {
        self.make_storage_request(
            method.clone(),
            r.build_url(Url::parse(&self.tsc.api_endpoint(&self.http_client)?)?)?,
        )
    }

    fn fetch_info<T>(&self, path: &str) -> error::Result<T>
    where
        for<'a> T: serde::de::Deserialize<'a>,
    {
        let mut resp = self.relative_storage_request(Method::Get, path)?;
        let result: T = resp.json()?;
        Ok(result)
    }

    fn update_timestamp(&self, hs: &header::Headers) {
        if let Some(ts) = hs.get::<XWeaveTimestamp>().map(|h| **h) {
            self.timestamp.set(ts);
        } else {
            // Should we complain more here?
            warn!("No X-Weave-Timestamp from storage server!");
        }
    }

    pub fn new_post_queue<'a, F: PostResponseHandler>(
        &'a self,
        coll: &str,
        config: &InfoConfiguration,
        ts: ServerTimestamp,
        on_response: F,
    ) -> error::Result<PostQueue<PostWrapper<'a>, F>> {
        let pw = PostWrapper {
            client: self,
            coll: coll.into(),
        };
        Ok(PostQueue::new(config, ts, pw, on_response))
    }
}

pub struct PostWrapper<'a> {
    client: &'a Sync15StorageClient,
    coll: String,
}

impl<'a> BatchPoster for PostWrapper<'a> {
    fn post<T, O>(
        &self,
        bytes: &[u8],
        xius: ServerTimestamp,
        batch: Option<String>,
        commit: bool,
        _: &PostQueue<T, O>,
    ) -> error::Result<PostResponse> {
        let url = CollectionRequest::new(self.coll.clone())
            .batch(batch)
            .commit(commit)
            .build_url(Url::parse(&self.client
                .tsc
                .api_endpoint(&self.client.http_client)?)?)?;

        let mut req = self.client.build_request(Method::Post, url)?;
        req.headers_mut().set(header::ContentType::json());
        req.headers_mut().set(XIfUnmodifiedSince(xius));
        // It's very annoying that we need to copy the body here, the request
        // shouldn't need to take ownership of it...
        *req.body_mut() = Some(Vec::from(bytes).into());
        let mut resp = self.client.exec_request(req, false)?;
        Ok(PostResponse::from_response(&mut resp)?)
    }
}
