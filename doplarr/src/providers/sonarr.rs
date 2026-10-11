use super::*;
use crate::{config::BackendConfig, discord::MAX_DROPDOWN_OPTIONS};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use sonarr_api::{
    apis::{
        Error as SonarrApiError,
        command_api::api_v3_command_post_custom,
        configuration::{ApiKey, Configuration},
        quality_profile_api::api_v3_qualityprofile_get,
        root_folder_api::api_v3_rootfolder_get,
        series_api::{api_v3_series_id_get, api_v3_series_id_put, api_v3_series_post},
        series_lookup_api::api_v3_series_lookup_get,
    },
    commands::SeasonSearchCommand,
    models::{
        AddSeriesOptions, NewItemMonitorTypes, QualityProfileResource, RootFolderResource,
        SeasonResource, SeriesResource, SeriesTypes,
    },
};
use tracing::{debug, error, info, trace, warn};

/// Helper function to log detailed error information from Sonarr API responses
fn log_api_error<T: std::fmt::Debug>(err: &SonarrApiError<T>, context: &str) {
    match err {
        SonarrApiError::ResponseError(response) => {
            super::api_logging::log_api_error_details(response.status, &response.content, context);
            if let Some(ref entity) = response.entity {
                debug!("Parsed error entity: {:#?}", entity);
            }
        }
        SonarrApiError::Reqwest(e) => {
            error!("{} - Reqwest error: {}", context, e);
        }
        SonarrApiError::Serde(e) => {
            error!("{} - Serialization error: {}", context, e);
        }
        SonarrApiError::Io(e) => {
            error!("{} - IO error: {}", context, e);
        }
    }
}

fn api_error<E>(err: SonarrApiError<E>, context: &str) -> anyhow::Error
where
    E: std::fmt::Debug + Send + Sync + 'static,
{
    log_api_error(&err, context);
    let status = match &err {
        SonarrApiError::ResponseError(response) => Some(response.status),
        _ => None,
    };
    let err = anyhow::Error::from(err);
    match status {
        Some(status) => err.context(BackendHttpError(status)),
        None => err,
    }
}

/// Treat a 2xx response whose body fails to parse as success - by the time we're
/// reading the body, Sonarr has already applied the change
fn tolerate_response_parse_error<T, E>(
    result: std::result::Result<T, SonarrApiError<E>>,
    context: &str,
) -> Result<Option<T>>
where
    E: std::fmt::Debug + Send + Sync + 'static,
{
    match result {
        Ok(x) => Ok(Some(x)),
        Err(SonarrApiError::Serde(e)) => {
            warn!(
                "{} - succeeded, but the response body failed to parse: {}",
                context, e
            );
            Ok(None)
        }
        Err(e) => Err(api_error(e, context)),
    }
}

#[derive(Debug, Clone)]
pub struct Sonarr {
    config: Configuration,
    details: Details,
    /// Whether Season 0 (specials) can be requested for existing series
    allow_specials: bool,
    /// Whether to offer an "All Seasons" option (all current + future seasons)
    allow_all_seasons: bool,
}

#[derive(Debug, Clone)]
// All the details we want to collect
pub struct Details {
    rootfolders: Vec<RootFolderResource>,
    quality_profiles: Vec<QualityProfileResource>,
    /// Config-pinned series type; when unset, it's auto-detected per series
    series_type: Option<SeriesTypes>,
    season_folder: Option<bool>,
}

#[derive(Debug)]
// The final details needed to complete the request
pub struct SelectedDetails {
    pub rootfolder_path: Option<String>, // Only for new series - existing series inherit
    pub quality_profile_id: Option<i32>, // Only for new series
    pub series_type: Option<SeriesTypes>, // Only for new series
    pub season_folder: Option<bool>,     // Only for new series - existing series inherit
    /// Season numbers the user chose to monitor (both new and existing series)
    pub season_numbers: Vec<i32>,
    /// User chose "All Seasons" - monitor all current + future seasons
    pub all_seasons: bool,
}

impl Sonarr {
    #[allow(clippy::too_many_arguments)]
    /// Builds the Sonarr connection and attempts to use it
    pub async fn new(
        base_path: String,
        key: String,
        quality_profile: Option<String>,
        rootfolder: Option<String>,
        series_type: Option<SeriesTypes>,
        season_folder: Option<bool>,
        allow_specials: bool,
        allow_all_seasons: bool,
        client: reqwest::Client,
    ) -> Result<Self> {
        // Log connection before moving base_path
        info!("Connecting to Sonarr at {}", base_path);

        // Build the API config
        let config = Configuration {
            base_path,
            user_agent: None,
            client,
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: Some(ApiKey { prefix: None, key }),
        };

        // Grab the additional details and use the config data to filter

        // First query the things we have to check (this will fail if we can't connect to the server anyway)
        let mut rootfolders = api_v3_rootfolder_get(&config)
            .await
            .map_err(|e| api_error(e, "Failed to get root folders from Sonarr"))?;
        trace!("Retrieved {} root folders", rootfolders.len());

        let mut quality_profiles = api_v3_qualityprofile_get(&config)
            .await
            .map_err(|e| api_error(e, "Failed to get quality profiles from Sonarr"))?;
        trace!("Retrieved {} quality profiles", quality_profiles.len());

        // Select rootfolder if given
        if let Some(rf) = rootfolder {
            // Get the index of the selection
            let rf_idx = rootfolders
                .iter()
                .position(|x| matches!(&x.path, Some(Some(path)) if path == &rf))
                .with_context(|| {
                    let available = rootfolders
                        .iter()
                        .filter_map(|x| x.path.as_ref().and_then(|inner| inner.as_deref()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!(
                        "Root folder '{}' not found. Available options: [{}]",
                        rf, available
                    )
                })?;
            let selected = rootfolders.swap_remove(rf_idx);
            rootfolders = vec![selected];
        }

        // Select quality profile if given
        if let Some(qp) = quality_profile {
            // Get the index of the selection
            let qp_idx = quality_profiles
                .iter()
                .position(|x| matches!(&x.name, Some(Some(name)) if name == &qp))
                .with_context(|| {
                    let available = quality_profiles
                        .iter()
                        .filter_map(|x| x.name.as_ref().and_then(|inner| inner.as_deref()))
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

        // Build the details
        let details = Details {
            rootfolders,
            quality_profiles,
            series_type,
            season_folder,
        };

        Ok(Self {
            config,
            details,
            allow_specials,
            allow_all_seasons,
        })
    }

    #[allow(irrefutable_let_patterns)]
    pub async fn connect(backend: BackendConfig, client: reqwest::Client) -> Result<Self> {
        if let BackendConfig::Sonarr {
            url,
            api_key,
            quality_profile,
            rootfolder,
            series_type,
            season_folders,
            allow_specials,
            allow_all_seasons,
        } = backend
        {
            Self::new(
                url,
                api_key,
                quality_profile,
                rootfolder,
                series_type,
                season_folders,
                allow_specials.unwrap_or(false),
                allow_all_seasons.unwrap_or(true),
                client,
            )
            .await
        } else {
            bail!("Configured backend not for Sonarr");
        }
    }

    /// Builds the multi-select season picker, or `None` when the series exposes
    /// no requestable seasons (after applying the specials filter). Already-
    /// monitored seasons are shown but tagged, so users see the full list.
    fn build_season_picker(&self, media: &SeriesResource) -> Option<RequestDetails> {
        let Some(Some(seasons)) = &media.seasons else {
            return None;
        };

        // A series only has a real monitoring state once it exists in Sonarr.
        // For a fresh lookup result (no id) Sonarr reports every season as
        // monitored by default, which is not a true "already monitored" state -
        // so only trust the flag for an existing series.
        let series_exists = media.id.is_some();

        let mut seasons: Vec<&SeasonResource> = seasons
            .iter()
            .filter(|s| self.allow_specials || s.season_number.unwrap_or(0) != 0)
            .collect();

        if seasons.is_empty() {
            return None;
        }

        // Ascending season order (1, 2, 3, ...), matching the Seerr picker
        seasons.sort_by_key(|s| s.season_number.unwrap_or(0));

        let mut options: Vec<DropdownOption> = Vec::new();

        // "All Seasons" leads the list and monitors all current + future seasons
        if self.allow_all_seasons {
            options.push(DropdownOption {
                title: "All Seasons".to_string(),
                description: Some("Includes future seasons".to_string()),
                id: Some(SelectableId::Integer(ALL_SEASONS_ID)),
            });
        }

        // Reserve a slot for the "All Seasons" entry against Discord's option cap
        let season_capacity = MAX_DROPDOWN_OPTIONS - options.len();
        if seasons.len() > season_capacity {
            debug!(
                total = seasons.len(),
                showing = season_capacity,
                "Truncating season list to fit Discord dropdown limit"
            );
        }

        options.extend(seasons.into_iter().take(season_capacity).map(|s| {
            let n = s.season_number.unwrap_or(0);
            // Bare number as the label (matching the Seerr picker), with any
            // status tags in the subtitle.
            let mut tags = Vec::new();
            if n == 0 {
                tags.push("Specials");
            }
            if series_exists && s.monitored.unwrap_or(false) {
                tags.push("Already monitored");
            }
            let description = (!tags.is_empty()).then(|| tags.join(" · "));
            DropdownOption {
                title: n.to_string(),
                description,
                id: Some(SelectableId::Integer(n)),
            }
        }));

        Some(RequestDetails {
            title: "Seasons".to_string(),
            options,
            metadata: Some(field_keys::SEASON.to_string()),
            selected_indices: vec![],
            field_type: FieldType::MultiSelect,
            always_show: true,
        })
    }
}

/// Helper function to get to and from stringified references
fn deserialize_from_string<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(&format!("\"{}\"", s))
        .with_context(|| format!("Failed to deserialize enum variant: {}", s))
}

/// Returns the requested seasons that aren't already monitored on the series.
/// An empty result means every requested season was already monitored.
fn seasons_to_monitor(requested: &[i32], already_monitored: &[i32]) -> Vec<i32> {
    requested
        .iter()
        .copied()
        .filter(|n| !already_monitored.contains(n))
        .collect()
}

/// Renders a list of season numbers for display, e.g. "Season 3" or
/// "Seasons 1, 2, 3". Empty input yields an empty string.
fn format_seasons(nums: &[i32]) -> String {
    let mut nums = nums.to_vec();
    nums.sort_unstable();
    match nums.as_slice() {
        [] => String::new(),
        [n] => format!("Season {n}"),
        _ => format!(
            "Seasons {}",
            nums.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

mod field_keys {
    pub const ROOT_FOLDER: &str = "sonarr:root_folder";
    pub const SERIES_TYPE: &str = "sonarr:series_type";
    pub const QUALITY_PROFILE: &str = "sonarr:quality_profile";
    pub const SEASON_FOLDER: &str = "sonarr:season_folder";
    pub const SEASON: &str = "sonarr:season";
}

impl From<Details> for Vec<RequestDetails> {
    fn from(details: Details) -> Vec<RequestDetails> {
        let quality_profile_options = details
            .quality_profiles
            .iter()
            .filter_map(|x| {
                let name = x.name.clone().flatten();
                if name.is_none() {
                    warn!("Skipping quality profile with no name (id: {:?})", x.id);
                }
                name.map(|n| DropdownOption {
                    title: n,
                    description: None,
                    id: x.id.map(SelectableId::Integer),
                })
            })
            .collect();

        let quality_profile_details = RequestDetails {
            title: "Quality Profile".to_string(),
            options: quality_profile_options,
            metadata: Some(field_keys::QUALITY_PROFILE.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Dropdown,
            always_show: false,
        };

        let rootfolder_options = details
            .rootfolders
            .iter()
            .filter_map(|x| {
                let path = x.path.clone().flatten();
                if path.is_none() {
                    warn!("Skipping root folder with no path (id: {:?})", x.id);
                }
                path.map(|p| DropdownOption {
                    title: p,
                    description: None,
                    id: x.id.map(SelectableId::Integer),
                })
            })
            .collect();

        let rootfolder_details = RequestDetails {
            title: "Root Folder".to_string(),
            options: rootfolder_options,
            metadata: Some(field_keys::ROOT_FOLDER.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Dropdown,
            always_show: false,
        };

        // Season folder boolean option - show both if None, or just the config value if Some
        let season_folder_options = match details.season_folder {
            Some(value) => {
                // Config default - show only that value
                vec![DropdownOption {
                    title: if value { "Yes" } else { "No" }.to_string(),
                    description: None,
                    id: Some(SelectableId::Boolean(value)),
                }]
            }
            None => {
                // No config default - show both for user selection
                vec![
                    DropdownOption {
                        title: "Yes".to_string(),
                        description: None,
                        id: Some(SelectableId::Boolean(true)),
                    },
                    DropdownOption {
                        title: "No".to_string(),
                        description: None,
                        id: Some(SelectableId::Boolean(false)),
                    },
                ]
            }
        };

        let season_folder_details = RequestDetails {
            title: "Use Season Folders".to_string(),
            options: season_folder_options,
            metadata: Some(field_keys::SEASON_FOLDER.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Boolean,
            always_show: false,
        };

        vec![
            rootfolder_details,
            quality_profile_details,
            season_folder_details,
        ]
    }
}

impl TryFrom<Vec<RequestDetails>> for SelectedDetails {
    type Error = anyhow::Error;

    fn try_from(details: Vec<RequestDetails>) -> Result<Self> {
        let mut root_folder_path = None;
        let mut quality_profile_id = None;
        let mut series_type = None;
        let mut season_folder = None;
        let mut season_numbers = Vec::new();
        let mut all_seasons = false;

        for detail in &details {
            // The season picker is multi-select; collect every chosen season.
            if detail.metadata.as_deref() == Some(field_keys::SEASON) {
                for opt in detail.selected_options() {
                    match &opt.id {
                        Some(SelectableId::Integer(ALL_SEASONS_ID)) => all_seasons = true,
                        Some(SelectableId::Integer(i)) => season_numbers.push(*i),
                        other => bail!("Season must have an integer ID, got {other:?}"),
                    }
                }
                continue;
            }

            let Some(selection) = detail.selected_option() else {
                bail!("No option was selected for '{}'", detail.title);
            };

            match detail.metadata.as_deref() {
                Some(field_keys::ROOT_FOLDER) => {
                    root_folder_path = Some(selection.title.clone());
                }
                Some(field_keys::QUALITY_PROFILE) => {
                    quality_profile_id = match &selection.id {
                        Some(SelectableId::Integer(i)) => Some(*i),
                        other => bail!("Quality profile must have an integer ID, got {other:?}"),
                    };
                }
                Some(field_keys::SERIES_TYPE) => {
                    series_type = match &selection.id {
                        Some(SelectableId::String(s)) => Some(deserialize_from_string(s)?),
                        other => bail!("Series type must have a string ID, got {other:?}"),
                    };
                }
                Some(field_keys::SEASON_FOLDER) => {
                    season_folder = match &selection.id {
                        Some(SelectableId::Boolean(b)) => Some(*b),
                        other => bail!("Season folder must have a boolean ID, got {other:?}"),
                    };
                }
                other => bail!("Unknown metadata key: {other:?}"),
            }
        }

        Ok(Self {
            rootfolder_path: root_folder_path, // Optional - only for new series
            quality_profile_id,                // Optional - only for new series
            series_type,                       // Optional - only for new series
            season_folder,                     // Optional - only for new series
            season_numbers,
            all_seasons,
        })
    }
}

impl MediaItem for SeriesResource {
    fn to_dropdown(&self) -> DropdownOption {
        DropdownOption {
            title: self.title.clone().flatten().unwrap_or_default(),
            description: self.year.map(|y| y.to_string()),
            id: self.id.map(SelectableId::Integer),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[async_trait]
impl MediaBackend for Sonarr {
    async fn search(&self, term: &str) -> Result<Vec<Box<dyn MediaItem>>> {
        info!("Searching Sonarr for series: {}", term);
        let results = api_v3_series_lookup_get(&self.config, Some(term))
            .await
            .map_err(|e| api_error(e, "Failed to search Sonarr"))?;
        debug!("Found {} series results", results.len());
        Ok(results
            .into_iter()
            .map(|s| Box::new(s) as Box<dyn MediaItem>)
            .collect())
    }

    fn early_stop(&self, media: &dyn MediaItem) -> bool {
        let Some(media) = media.as_any().downcast_ref::<SeriesResource>() else {
            error!("early_stop called with wrong media type for Sonarr backend");
            return false;
        };

        // Check if series exists and all requestable seasons are already monitored
        // (when specials are disabled, an unmonitored Season 0 doesn't count)
        if let Some(id) = media.id
            && let Some(Some(ref seasons)) = media.seasons
        {
            let all_monitored = seasons
                .iter()
                .filter(|s| self.allow_specials || s.season_number.unwrap_or(0) != 0)
                .all(|s| s.monitored.unwrap_or(false));

            if all_monitored && !seasons.is_empty() {
                info!(series_id = id, "Series already fully monitored");
                return true;
            }
        }

        // Otherwise, allow the request to proceed
        // Users can select individual unmonitored seasons to add
        false
    }

    fn display_info(&self, media: &dyn MediaItem) -> MediaDisplayInfo {
        let Some(media) = media.as_any().downcast_ref::<SeriesResource>() else {
            error!("display_info called with wrong media type for Sonarr backend");
            return MediaDisplayInfo {
                title: String::new(),
                subtitle: None,
                description: None,
                thumbnail_url: None,
            };
        };

        MediaDisplayInfo {
            title: media.title.clone().flatten().unwrap_or_default(),
            subtitle: media.year.map(|y| y.to_string()),
            description: media.overview.clone().flatten(),
            thumbnail_url: media.remote_poster.clone().flatten(),
        }
    }

    async fn additional_details(&self, media: &dyn MediaItem) -> Result<Vec<RequestDetails>> {
        let media = media
            .as_any()
            .downcast_ref::<SeriesResource>()
            .context("Invalid media type for Sonarr")?;

        let mut details: Vec<RequestDetails> = self.details.clone().into();

        if media.id.is_some() {
            // Existing series: every add-time setting is inherited, so the only
            // thing to collect is which seasons to monitor.
            debug!("Series already exists, showing only the season picker");
            details.clear();
        } else {
            // New series: series type is Sonarr arcana most requesters won't
            // understand, so don't ask - use the config pin if present,
            // otherwise auto-detect anime from the lookup's genres
            let series_type = self.details.series_type.unwrap_or_else(|| {
                let is_anime = matches!(&media.genres, Some(Some(genres))
                    if genres.iter().any(|g| g.eq_ignore_ascii_case("anime")));
                if is_anime {
                    SeriesTypes::Anime
                } else {
                    SeriesTypes::Standard
                }
            });
            debug!(series_type = %series_type, "Resolved series type");

            let title = match series_type {
                SeriesTypes::Standard => "Standard",
                SeriesTypes::Daily => "Daily",
                SeriesTypes::Anime => "Anime",
            };
            details.push(RequestDetails {
                title: "Series Type".to_string(),
                options: vec![DropdownOption {
                    title: title.to_string(),
                    description: None,
                    id: Some(SelectableId::String(series_type.to_string())),
                }],
                metadata: Some(field_keys::SERIES_TYPE.to_string()),
                selected_indices: vec![],
                field_type: FieldType::Dropdown,
                always_show: false,
            });
        }

        // Season picker (multi-select) for both new and existing series. We
        // show every requestable season - including ones already monitored on
        // an existing series - and reject already-monitored picks at request
        // time rather than hiding them from the list.
        let Some(season_picker) = self.build_season_picker(media) else {
            bail!(UserFacingError("No requestable seasons found.".into()));
        };
        details.push(season_picker);

        Ok(details)
    }

    async fn request(
        &self,
        details: Vec<RequestDetails>,
        media: Box<dyn MediaItem>,
        _requester_discord_id: u64,
    ) -> Result<()> {
        let selected = SelectedDetails::try_from(details)?;

        // Downcast to concrete type
        let mut media = *media
            .into_any()
            .downcast::<SeriesResource>()
            .map_err(|_| anyhow::anyhow!("Invalid media type for Sonarr"))?;

        info!(
            "Requesting series: {} (tvdb_id: {:?})",
            media.title.clone().flatten().unwrap_or_default(),
            media.tvdb_id
        );

        if selected.season_numbers.is_empty() && !selected.all_seasons {
            bail!(UserFacingError("No seasons were selected.".into()));
        }

        // Existing series in Sonarr (has an ID)
        if let Some(id) = media.id {
            info!(series_id = id, "Series already exists in Sonarr");

            // Get the current series data
            let mut existing_series = api_v3_series_id_get(&self.config, id, None)
                .await
                .map_err(|e| api_error(e, "Failed to get existing series from Sonarr"))?;

            // Determine which seasons still need monitoring (additive only).
            // For "All Seasons" that's every currently-unmonitored season
            // (specials gated); otherwise the picks minus what's already on.
            let to_monitor: Vec<i32> = if selected.all_seasons {
                existing_series
                    .seasons
                    .as_ref()
                    .and_then(|s| s.as_ref())
                    .map(|seasons| {
                        seasons
                            .iter()
                            .filter(|s| self.allow_specials || s.season_number.unwrap_or(0) != 0)
                            .filter(|s| !s.monitored.unwrap_or(false))
                            .filter_map(|s| s.season_number)
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                let already_monitored: Vec<i32> = existing_series
                    .seasons
                    .as_ref()
                    .and_then(|s| s.as_ref())
                    .map(|seasons| {
                        seasons
                            .iter()
                            .filter(|s| s.monitored.unwrap_or(false))
                            .filter_map(|s| s.season_number)
                            .collect()
                    })
                    .unwrap_or_default();
                seasons_to_monitor(&selected.season_numbers, &already_monitored)
            };

            // "All Seasons" also keeps future seasons monitored, so it's still a
            // meaningful change even when every current season is already on.
            if to_monitor.is_empty() && !selected.all_seasons {
                bail!(UserFacingError(format!(
                    "{} already monitored.",
                    format_seasons(&selected.season_numbers)
                )));
            }
            debug!(
                ?to_monitor,
                all_seasons = selected.all_seasons,
                "Adding seasons to monitoring"
            );

            // Mark the seasons monitored (additive only - never unmonitor)
            let Some(Some(seasons)) = existing_series.seasons.as_mut() else {
                bail!("Series has no seasons to update");
            };
            for n in &to_monitor {
                match seasons.iter_mut().find(|s| s.season_number == Some(*n)) {
                    Some(season) => season.monitored = Some(true),
                    None => bail!("Season {n} not found in series"),
                }
            }
            existing_series.monitored = Some(true);
            if selected.all_seasons {
                existing_series.monitor_new_items = Some(NewItemMonitorTypes::All);
            }

            trace!("Updated series object: {:#?}", existing_series);

            tolerate_response_parse_error(
                api_v3_series_id_put(&self.config, &id.to_string(), None, Some(existing_series))
                    .await,
                "Failed to update series in Sonarr",
            )?;

            // Trigger a search scoped to each newly monitored season
            for n in &to_monitor {
                let search_command = SeasonSearchCommand::new(id, *n);
                let result = tolerate_response_parse_error(
                    api_v3_command_post_custom(&self.config, &search_command).await,
                    "Failed to trigger season search",
                )?;
                info!(season = n, command_id = ?result.and_then(|r| r.id), "Season search queued");
            }
        } else {
            info!("Series is new, adding to Sonarr");

            let rootfolder_path = selected
                .rootfolder_path
                .context("No root folder was selected for a new series")?;
            let season_folder = selected
                .season_folder
                .context("No season folder choice was selected for a new series")?;
            let quality_profile_id = selected
                .quality_profile_id
                .context("No quality profile was selected for a new series")?;
            let series_type = selected
                .series_type
                .context("No series type was selected for a new series")?;

            debug!(
                "Request details - rootfolder: {}, quality_profile_id: {:?}, series_type: {:?}, season_folder: {}, seasons: {:?}",
                rootfolder_path,
                quality_profile_id,
                series_type,
                season_folder,
                selected.season_numbers
            );

            // Monitor exactly the requested seasons (or all of them, specials
            // gated, for "All Seasons"); everything else off. Like Seerr, the
            // explicit season list drives monitoring rather than the monitor
            // enum.
            if let Some(Some(seasons)) = media.seasons.as_mut() {
                for season in seasons.iter_mut() {
                    let monitored = match season.season_number {
                        Some(n) if selected.all_seasons => self.allow_specials || n != 0,
                        Some(n) => selected.season_numbers.contains(&n),
                        None => false,
                    };
                    season.monitored = Some(monitored);
                }
            }

            media.add_options = Some(Box::new(AddSeriesOptions {
                ignore_episodes_with_files: Some(true),
                ignore_episodes_without_files: Some(false),
                monitor: None,
                search_for_cutoff_unmet_episodes: Some(false),
                search_for_missing_episodes: Some(true),
            }));
            media.root_folder_path = Some(Some(rootfolder_path));
            media.season_folder = Some(season_folder);
            media.monitored = Some(true);
            media.quality_profile_id = Some(quality_profile_id);
            media.series_type = Some(series_type);
            // Keep grabbing future seasons too when "All Seasons" was chosen
            if selected.all_seasons {
                media.monitor_new_items = Some(NewItemMonitorTypes::All);
            }

            trace!("Full media object: {:#?}", media);

            tolerate_response_parse_error(
                api_v3_series_post(&self.config, Some(media)).await,
                "Failed to add series to Sonarr",
            )?;
        }

        Ok(())
    }

    fn success_message(&self, details: &[RequestDetails], media: &dyn MediaItem) -> SuccessMessage {
        let Some(media) = media.as_any().downcast_ref::<SeriesResource>() else {
            error!("success_message called with wrong media type for Sonarr backend");
            return SuccessMessage {
                summary: "Request submitted".into(),
                description: "Will be downloaded when available.".into(),
                thumbnail_url: None,
                embed_data: None,
            };
        };

        let title = media.title.clone().flatten().unwrap_or_default();
        let year = media.year.unwrap_or_default();

        // List the requested seasons, e.g. " (Seasons 1, 2, 3)"
        let season_nums: Vec<i32> = details
            .iter()
            .find(|d| d.metadata.as_deref() == Some(field_keys::SEASON))
            .map(|d| {
                d.selected_options()
                    .filter_map(|o| match &o.id {
                        Some(SelectableId::Integer(n)) => Some(*n),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let detail_text = if season_nums.contains(&ALL_SEASONS_ID) {
            " (All Seasons)".to_string()
        } else {
            match format_seasons(&season_nums) {
                s if s.is_empty() => String::new(),
                s => format!(" ({s})"),
            }
        };

        let overview = media.overview.clone().flatten().unwrap_or_default();
        let genres: Vec<String> = media.genres.clone().flatten().unwrap_or_default();
        let external_url = media
            .tmdb_id
            .map(|id| format!("https://www.themoviedb.org/tv/{id}"));

        let embed_data = external_url.map(|external_url| EmbedData {
            title: format!("{title} ({year}){detail_text}"),
            media_type: "TV Series",
            overview: truncate_for_embed(&overview),
            poster_url: media.remote_poster.clone().flatten().unwrap_or_default(),
            genres,
            runtime_minutes: media.runtime.map(|r| r as u32),
            studio_or_network: media.network.clone().flatten(),
            director: None,
            external_url,
        });

        SuccessMessage {
            summary: format!("{title} ({year}){detail_text}"),
            description: "Will be downloaded when available.".to_string(),
            thumbnail_url: media.remote_poster.clone().flatten(),
            embed_data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(
        metadata: &str,
        title: &str,
        id: SelectableId,
        field_type: FieldType,
        selected: bool,
    ) -> RequestDetails {
        RequestDetails {
            title: metadata.to_string(),
            options: vec![DropdownOption {
                title: title.to_string(),
                description: None,
                id: Some(id),
            }],
            selected_indices: if selected { vec![0] } else { vec![] },
            metadata: Some(metadata.to_string()),
            field_type,
            always_show: false,
        }
    }

    /// A multi-select season picker over the given season numbers, with the
    /// options at `selected` indices chosen.
    fn season_field(seasons: &[i32], selected: &[usize]) -> RequestDetails {
        RequestDetails {
            title: "Seasons".into(),
            options: seasons
                .iter()
                .map(|n| DropdownOption {
                    title: format!("Season {n}"),
                    description: None,
                    id: Some(SelectableId::Integer(*n)),
                })
                .collect(),
            selected_indices: selected.to_vec(),
            metadata: Some(field_keys::SEASON.to_string()),
            field_type: FieldType::MultiSelect,
            always_show: true,
        }
    }

    /// New-series flow: every field present and explicitly selected.
    fn full_details() -> Vec<RequestDetails> {
        use FieldType::Dropdown;
        vec![
            detail(
                field_keys::ROOT_FOLDER,
                "/tv",
                SelectableId::Integer(1),
                Dropdown,
                true,
            ),
            detail(
                field_keys::QUALITY_PROFILE,
                "HD",
                SelectableId::Integer(3),
                Dropdown,
                true,
            ),
            detail(
                field_keys::SERIES_TYPE,
                "Standard",
                SelectableId::String("standard".into()),
                Dropdown,
                true,
            ),
            detail(
                field_keys::SEASON_FOLDER,
                "Yes",
                SelectableId::Boolean(true),
                Dropdown,
                true,
            ),
            season_field(&[1], &[0]),
        ]
    }

    #[test]
    fn try_from_all_selected() {
        let selected = SelectedDetails::try_from(full_details()).unwrap();
        assert_eq!(selected.rootfolder_path.as_deref(), Some("/tv"));
        assert_eq!(selected.quality_profile_id, Some(3));
        assert_eq!(selected.series_type, Some(SeriesTypes::Standard));
        assert_eq!(selected.season_folder, Some(true));
        assert_eq!(selected.season_numbers, vec![1]);
    }

    #[test]
    fn try_from_collects_multiple_seasons() {
        let mut details = full_details();
        // Replace the season picker with one that has three seasons, two chosen.
        *details.last_mut().unwrap() = season_field(&[3, 2, 1], &[0, 2]);
        let selected = SelectedDetails::try_from(details).unwrap();
        assert_eq!(selected.season_numbers, vec![3, 1]);
        assert!(!selected.all_seasons);
    }

    #[test]
    fn try_from_all_seasons_sentinel() {
        let mut details = full_details();
        // "All Seasons" option (sentinel id) selected, leading the list.
        *details.last_mut().unwrap() = season_field(&[ALL_SEASONS_ID, 2, 1], &[0]);
        let selected = SelectedDetails::try_from(details).unwrap();
        assert!(selected.all_seasons);
        assert!(selected.season_numbers.is_empty());
    }

    #[test]
    fn try_from_preset_fields_are_auto_selected() {
        // Admin presets root folder and quality profile, collapsing each to a
        // single hidden option the user never selects. Must still resolve.
        let mut details = full_details();
        details[0].selected_indices = vec![]; // rootfolder preset
        details[1].selected_indices = vec![]; // quality profile preset
        let selected = SelectedDetails::try_from(details).unwrap();
        assert_eq!(selected.rootfolder_path.as_deref(), Some("/tv"));
        assert_eq!(selected.quality_profile_id, Some(3));
    }

    #[test]
    fn try_from_unselected_multi_option_field_errors() {
        let mut details = full_details();
        // Quality profile with two options and nothing selected must error.
        details[1].options.push(DropdownOption {
            title: "4K".into(),
            description: None,
            id: Some(SelectableId::Integer(4)),
        });
        details[1].selected_indices = vec![];
        assert!(SelectedDetails::try_from(details).is_err());
    }

    #[test]
    fn seasons_to_monitor_skips_already_monitored() {
        assert_eq!(seasons_to_monitor(&[1, 2, 3], &[2]), vec![1, 3]);
        assert_eq!(seasons_to_monitor(&[1, 2], &[5]), vec![1, 2]);
    }

    #[test]
    fn seasons_to_monitor_empty_when_all_dups() {
        assert!(seasons_to_monitor(&[1, 2], &[1, 2, 3]).is_empty());
    }

    #[test]
    fn format_seasons_renders() {
        assert_eq!(format_seasons(&[]), "");
        assert_eq!(format_seasons(&[3]), "Season 3");
        assert_eq!(format_seasons(&[3, 1, 2]), "Seasons 1, 2, 3");
    }

    /// A Sonarr backend with no live connection, for picker tests.
    fn test_sonarr(allow_specials: bool, allow_all_seasons: bool) -> Sonarr {
        // Skip system CA loading so the test works in sandboxed environments (e.g. Nix).
        let client = reqwest::ClientBuilder::new()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        Sonarr {
            config: Configuration {
                base_path: "http://localhost:8989".to_owned(),
                user_agent: None,
                client,
                basic_auth: None,
                oauth_access_token: None,
                bearer_access_token: None,
                api_key: None,
            },
            details: Details {
                rootfolders: vec![],
                quality_profiles: vec![],
                series_type: None,
                season_folder: None,
            },
            allow_specials,
            allow_all_seasons,
        }
    }

    /// A SeriesResource with the given Sonarr id and monitored-flagged seasons.
    fn series_with_seasons(id: Option<i32>, seasons: &[(i32, bool)]) -> SeriesResource {
        SeriesResource {
            id,
            seasons: Some(Some(
                seasons
                    .iter()
                    .map(|(n, monitored)| SeasonResource {
                        season_number: Some(*n),
                        monitored: Some(*monitored),
                        ..Default::default()
                    })
                    .collect(),
            )),
            ..Default::default()
        }
    }

    fn season_descriptions(picker: &RequestDetails) -> Vec<Option<String>> {
        picker
            .options
            .iter()
            .map(|o| o.description.clone())
            .collect()
    }

    #[test]
    fn picker_new_series_never_marks_seasons_already_monitored() {
        // A fresh lookup result (id = None) reports seasons as monitored by
        // default; the picker must not surface that as "Already monitored".
        let sonarr = test_sonarr(false, false);
        let media = series_with_seasons(None, &[(1, true), (2, true)]);
        let picker = sonarr.build_season_picker(&media).expect("picker");

        assert!(
            season_descriptions(&picker)
                .iter()
                .flatten()
                .all(|d| !d.contains("Already monitored")),
            "new series should have no already-monitored tags: {:?}",
            season_descriptions(&picker)
        );
    }

    #[test]
    fn picker_existing_series_marks_monitored_seasons() {
        // For a series already in Sonarr (id = Some), the monitored flag is real.
        let sonarr = test_sonarr(false, false);
        let media = series_with_seasons(Some(42), &[(1, true), (2, false)]);
        let picker = sonarr.build_season_picker(&media).expect("picker");

        let descs = season_descriptions(&picker);
        // allow_all_seasons is false, so options are seasons 1 then 2 in order.
        assert_eq!(descs[0].as_deref(), Some("Already monitored"));
        assert_eq!(descs[1], None);
    }
}
