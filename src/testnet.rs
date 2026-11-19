//! A transport for the tests of the download: it answers from a table, remembers what it was asked for, and never
//! touches a network.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use crate::ddi::fetch::{BoxFuture, Fetch};
use crate::ddi::http::{HttpError, Url};

/// The answers given for URLs (in the text of [`Url::as_text`]); any other URL is a 404.
#[derive(Debug, Default)]
pub(crate) struct FakeFetch {
    answers: Mutex<HashMap<String, Result<Vec<u8>, HttpError>>>,
    asked: Mutex<Vec<String>>,
}

impl FakeFetch {
    pub(crate) fn answer(&self, url: &str, body: Result<Vec<u8>, HttpError>) {
        self.answers.lock().unwrap_or_else(PoisonError::into_inner).insert(url.to_string(), body);
    }

    /// The URLs that were asked for, in order.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

impl Fetch for FakeFetch {
    fn get<'a>(
        &'a self,
        url: &'a Url,
        max_bytes: u64,
        progress: &'a mut (dyn FnMut(u64) + Send),
    ) -> BoxFuture<'a, Result<Vec<u8>, HttpError>> {
        Box::pin(async move {
            self.asked.lock().unwrap_or_else(PoisonError::into_inner).push(url.as_text());
            let answer = self.answers.lock().unwrap_or_else(PoisonError::into_inner).get(&url.as_text()).cloned();
            let body = answer.unwrap_or(Err(HttpError::Status(404)))?;
            // A real client refuses what is larger than the cap, and tells its progress as it reads.
            let len = u64::try_from(body.len()).unwrap();
            if len > max_bytes {
                return Err(HttpError::TooLarge { limit: max_bytes });
            }
            for step in 1..=4_u64 {
                progress(len * step / 4);
            }
            Ok(body)
        })
    }
}
