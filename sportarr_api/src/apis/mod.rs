pub mod configuration;
pub mod leagues_api;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Reqwest(reqwest::Error),
    Serde(serde_json::Error),
    ResponseError(ResponseContent),
}

#[derive(Debug)]
pub struct ResponseContent {
    pub status: reqwest::StatusCode,
    pub content: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Reqwest(e) => write!(f, "error in reqwest: {e}"),
            Error::Serde(e) => write!(f, "error in serde: {e}"),
            Error::ResponseError(r) => {
                write!(
                    f,
                    "error in response: status code {}: {}",
                    r.status, r.content
                )
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Reqwest(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serde(e)
    }
}
