use super::{configuration::Configuration, Error, ResponseContent};
use crate::models;

async fn execute<T: serde::de::DeserializeOwned>(req: reqwest::RequestBuilder) -> Result<T, Error> {
    let resp = req.send().await?;
    let status = resp.status();
    let content = resp.text().await?;

    if status.is_success() {
        Ok(serde_json::from_str(&content)?)
    } else {
        Err(Error::ResponseError(ResponseContent { status, content }))
    }
}

fn request(config: &Configuration, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
    let mut req = config
        .client
        .request(method, format!("{}{}", config.base_path, path));
    if let Some(ua) = &config.user_agent {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }
    if let Some(key) = &config.api_key {
        req = req.header("X-Api-Key", key);
    }
    req
}

/// `GET /api/leagues/search/{query}` - search the metadata catalog for
/// leagues to add.
pub async fn api_leagues_search_get(
    config: &Configuration,
    query: &str,
) -> Result<Vec<models::CatalogLeague>, Error> {
    // Path-segment encoding: form encoding would turn spaces into `+`, which
    // the server takes literally and multi-word searches silently miss.
    let encoded =
        percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC);
    execute(request(
        config,
        reqwest::Method::GET,
        &format!("/api/leagues/search/{encoded}"),
    ))
    .await
}

/// `GET /api/leagues` - leagues already in the library.
pub async fn api_leagues_get(config: &Configuration) -> Result<Vec<models::LibraryLeague>, Error> {
    execute(request(config, reqwest::Method::GET, "/api/leagues")).await
}

/// `GET /api/qualityprofile`
pub async fn api_qualityprofile_get(
    config: &Configuration,
) -> Result<Vec<models::QualityProfile>, Error> {
    execute(request(config, reqwest::Method::GET, "/api/qualityprofile")).await
}

/// `GET /api/rootfolder`
pub async fn api_rootfolder_get(config: &Configuration) -> Result<Vec<models::RootFolder>, Error> {
    execute(request(config, reqwest::Method::GET, "/api/rootfolder")).await
}

/// `POST /api/leagues` - add a league to the library.
pub async fn api_leagues_post(
    config: &Configuration,
    body: &models::AddLeagueRequest,
) -> Result<models::AddedLeague, Error> {
    execute(request(config, reqwest::Method::POST, "/api/leagues").json(body)).await
}
