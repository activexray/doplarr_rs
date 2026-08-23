use super::*;
use crate::config::BackendConfig;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use sportarr_api::{
    apis::{
        Error as SportarrApiError,
        configuration::Configuration,
        leagues_api::{
            api_leagues_get, api_leagues_post, api_leagues_search_get, api_qualityprofile_get,
        },
    },
    models::{AddLeagueRequest, CatalogLeague, QualityProfile},
};
use tracing::{debug, error, info};

mod field_keys {
    pub const QUALITY_PROFILE: &str = "sportarr:quality_profile";
}

fn log_api_error(err: &SportarrApiError, context: &str) {
    match err {
        SportarrApiError::ResponseError(response) => {
            super::api_logging::log_api_error_details(response.status, &response.content, context);
        }
        SportarrApiError::Reqwest(e) => {
            error!("{} - Reqwest error: {}", context, e);
        }
        SportarrApiError::Serde(e) => {
            error!("{} - Serialization error: {}", context, e);
        }
    }
}

#[derive(Debug, Clone)]
pub struct Sportarr {
    config: Configuration,
    details: Details,
}

#[derive(Debug, Clone)]
struct Details {
    quality_profiles: Vec<QualityProfile>,
}

/// A catalog league enriched with whether it already exists in the library.
/// Sportarr's catalog search doesn't carry library state, so `search`
/// cross-references the library list once per search.
#[derive(Debug, Clone)]
pub struct SportarrMedia {
    league: CatalogLeague,
    /// (library id, monitored) when the league is already added
    existing: Option<(i32, bool)>,
}

impl MediaItem for SportarrMedia {
    fn to_dropdown(&self) -> DropdownOption {
        let mut description = self.league.str_sport.clone();
        if let Some(country) = &self.league.str_country
            && !country.is_empty()
        {
            description = format!("{description} · {country}");
        }
        DropdownOption {
            title: self.league.str_league.clone(),
            description: Some(description),
            id: Some(SelectableId::String(self.league.id_league.clone())),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl Sportarr {
    /// Builds the Sportarr connection and attempts to use it
    pub async fn new(
        base_path: String,
        key: String,
        quality_profile: Option<String>,
        client: reqwest::Client,
    ) -> Result<Self> {
        info!("Connecting to Sportarr at {}", base_path);

        let config = Configuration {
            base_path,
            user_agent: None,
            client,
            api_key: Some(key),
        };

        // This will fail fast if the server is unreachable or the key is wrong
        let mut quality_profiles = api_qualityprofile_get(&config).await.inspect_err(|e| {
            log_api_error(e, "Failed to get quality profiles from Sportarr");
        })?;
        debug!("Retrieved {} quality profiles", quality_profiles.len());

        // Pin the quality profile if configured
        if let Some(qp) = quality_profile {
            let qp_idx = quality_profiles
                .iter()
                .position(|x| x.name == qp)
                .with_context(|| {
                    let available = quality_profiles
                        .iter()
                        .map(|x| x.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "Quality profile '{}' not found. Available options: [{}]",
                        qp, available
                    )
                })?;
            let selected = quality_profiles.swap_remove(qp_idx);
            quality_profiles = vec![selected];
        }

        Ok(Self {
            config,
            details: Details { quality_profiles },
        })
    }

    pub async fn connect(backend: BackendConfig, client: reqwest::Client) -> Result<Self> {
        if let BackendConfig::Sportarr {
            url,
            api_key,
            quality_profile,
        } = backend
        {
            Self::new(url, api_key, quality_profile, client).await
        } else {
            bail!("Configured backend not for Sportarr");
        }
    }
}

impl From<Details> for Vec<RequestDetails> {
    fn from(details: Details) -> Vec<RequestDetails> {
        let quality_profile_options = details
            .quality_profiles
            .iter()
            .map(|x| DropdownOption {
                title: x.name.clone(),
                description: None,
                id: Some(SelectableId::Integer(x.id)),
            })
            .collect();

        vec![RequestDetails {
            title: "Quality Profile".to_string(),
            options: quality_profile_options,
            metadata: Some(field_keys::QUALITY_PROFILE.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Dropdown,
            always_show: false,
        }]
    }
}

#[async_trait]
impl MediaBackend for Sportarr {
    async fn search(&self, term: &str) -> Result<Vec<Box<dyn MediaItem>>> {
        info!("Searching Sportarr for league: {}", term);
        let results = api_leagues_search_get(&self.config, term)
            .await
            .inspect_err(|e| {
                log_api_error(e, "Failed to search Sportarr");
            })?;
        debug!("Found {} league results", results.len());

        // Cross-reference the library so already-added leagues are known
        let library = api_leagues_get(&self.config).await.inspect_err(|e| {
            log_api_error(e, "Failed to list Sportarr library leagues");
        })?;

        Ok(results
            .into_iter()
            .map(|league| {
                let existing = library
                    .iter()
                    .find(|l| {
                        l.external_id.as_deref() == Some(league.id_league.as_str())
                            && !league.id_league.is_empty()
                    })
                    .map(|l| (l.id, l.monitored));
                Box::new(SportarrMedia { league, existing }) as Box<dyn MediaItem>
            })
            .collect())
    }

    fn early_stop(&self, media: &dyn MediaItem) -> bool {
        let Some(media) = media.as_any().downcast_ref::<SportarrMedia>() else {
            error!("early_stop called with wrong media type for Sportarr backend");
            return false;
        };

        if let Some((id, true)) = media.existing {
            info!(league_id = id, "League already monitored");
            return true;
        }

        false
    }

    fn display_info(&self, media: &dyn MediaItem) -> MediaDisplayInfo {
        let Some(media) = media.as_any().downcast_ref::<SportarrMedia>() else {
            error!("display_info called with wrong media type for Sportarr backend");
            return MediaDisplayInfo {
                title: String::new(),
                subtitle: None,
                description: None,
                thumbnail_url: None,
            };
        };

        let mut subtitle = media.league.str_sport.clone();
        if let Some(year) = &media.league.int_formed_year
            && !year.is_empty()
        {
            subtitle = format!("{subtitle} · est. {year}");
        }

        MediaDisplayInfo {
            title: media.league.str_league.clone(),
            subtitle: Some(subtitle),
            description: media.league.str_description_en.clone(),
            thumbnail_url: media
                .league
                .str_badge
                .clone()
                .or_else(|| media.league.str_poster.clone()),
        }
    }

    async fn additional_details(&self, media: &dyn MediaItem) -> Result<Vec<RequestDetails>> {
        let media = media
            .as_any()
            .downcast_ref::<SportarrMedia>()
            .context("Invalid media type for Sportarr")?;

        if let Some((_, false)) = media.existing {
            // Re-monitoring an existing league is a deliberate library
            // decision (it may have been unmonitored for a reason), so send
            // the user to Sportarr instead of silently flipping it here.
            bail!(UserFacingError(format!(
                "{} is already in the library but unmonitored. Enable monitoring in Sportarr to resume grabbing it.",
                media.league.str_league
            )));
        }

        // New league: the only decision to collect is the quality profile
        // (root folder and monitoring scope follow Sportarr's own defaults)
        Ok(self.details.clone().into())
    }

    async fn request(
        &self,
        details: Vec<RequestDetails>,
        media: Box<dyn MediaItem>,
        requester_discord_id: u64,
    ) -> Result<()> {
        let media = media
            .into_any()
            .downcast::<SportarrMedia>()
            .ok()
            .context("Invalid media type for Sportarr")?;

        let mut quality_profile_id = None;
        for detail in &details {
            let Some(selection) = detail.selected_option() else {
                bail!("No option was selected for '{}'", detail.title);
            };
            match detail.metadata.as_deref() {
                Some(field_keys::QUALITY_PROFILE) => {
                    quality_profile_id = match &selection.id {
                        Some(SelectableId::Integer(i)) => Some(*i),
                        other => bail!("Quality profile must have an integer ID, got {other:?}"),
                    };
                }
                other => bail!("Unknown metadata key: {other:?}"),
            }
        }

        info!(
            league = %media.league.str_league,
            requester = requester_discord_id,
            "Adding league to Sportarr"
        );

        let body = AddLeagueRequest {
            external_id: (!media.league.id_league.is_empty())
                .then(|| media.league.id_league.clone()),
            name: media.league.str_league.clone(),
            sport: media.league.str_sport.clone(),
            country: media.league.str_country.clone(),
            description: media.league.str_description_en.clone(),
            monitored: true,
            quality_profile_id,
        };

        api_leagues_post(&self.config, &body)
            .await
            .inspect_err(|e| {
                log_api_error(e, "Failed to add league to Sportarr");
            })?;

        Ok(())
    }

    fn success_message(
        &self,
        _details: &[RequestDetails],
        media: &dyn MediaItem,
    ) -> SuccessMessage {
        let Some(media) = media.as_any().downcast_ref::<SportarrMedia>() else {
            error!("success_message called with wrong media type for Sportarr backend");
            return SuccessMessage {
                summary: "Request submitted".into(),
                description: "Will be downloaded when available.".into(),
                thumbnail_url: None,
                embed_data: None,
            };
        };

        let overview = media.league.str_description_en.clone().unwrap_or_default();
        let external_url = media
            .league
            .str_website
            .clone()
            .filter(|w| !w.is_empty())
            .map(|w| {
                if w.starts_with("http") {
                    w
                } else {
                    format!("https://{w}")
                }
            })
            .unwrap_or_else(|| "https://github.com/Sportarr/Sportarr".to_string());

        SuccessMessage {
            summary: format!("{} has been requested!", media.league.str_league),
            description: "New events will be grabbed as they become available.".into(),
            thumbnail_url: media.league.str_badge.clone(),
            embed_data: Some(EmbedData {
                title: media.league.str_league.clone(),
                media_type: "league",
                overview,
                poster_url: media
                    .league
                    .str_poster
                    .clone()
                    .or_else(|| media.league.str_badge.clone())
                    .unwrap_or_default(),
                genres: vec![media.league.str_sport.clone()],
                runtime_minutes: None,
                studio_or_network: media.league.str_country.clone(),
                director: None,
                external_url,
            }),
        }
    }
}
