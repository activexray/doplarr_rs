use serde::{Deserialize, Serialize};

/// A league from the catalog search (`GET /api/leagues/search/{query}`).
/// Field names follow Sportarr's metadata-catalog conventions.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogLeague {
    #[serde(default)]
    pub id_league: String,
    #[serde(default)]
    pub str_league: String,
    #[serde(default)]
    pub str_sport: String,
    pub str_league_alternate: Option<String>,
    pub int_formed_year: Option<String>,
    pub str_country: Option<String>,
    #[serde(rename = "strDescriptionEN")]
    pub str_description_en: Option<String>,
    pub str_badge: Option<String>,
    pub str_logo: Option<String>,
    pub str_banner: Option<String>,
    pub str_poster: Option<String>,
    pub str_website: Option<String>,
}

/// A league already in the library (`GET /api/leagues`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryLeague {
    pub id: i32,
    pub external_id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub sport: String,
    #[serde(default)]
    pub monitored: bool,
    pub quality_profile_id: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityProfile {
    pub id: i32,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootFolder {
    pub id: i32,
    #[serde(default)]
    pub path: String,
}

/// Body for `POST /api/leagues`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AddLeagueRequest {
    pub external_id: Option<String>,
    pub name: String,
    pub sport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub monitored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_profile_id: Option<i32>,
}

/// Response of `POST /api/leagues` (subset).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddedLeague {
    /// Sportarr answers `POST /api/leagues` with a confirmation envelope,
    /// not the created entity: `{message, leagueId, monitored}`.
    pub league_id: Option<i32>,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub monitored: bool,
}
