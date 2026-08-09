use super::*;
use crate::{config::BackendConfig, discord::EARLY_STOP_MESSAGE};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use radarr_api::{
    apis::{
        Error as RadarrApiError,
        configuration::{ApiKey, Configuration},
        movie_api::{api_v3_movie_id_get, api_v3_movie_post},
        movie_lookup_api::api_v3_movie_lookup_get,
        quality_profile_api::api_v3_qualityprofile_get,
        queue_details_api::api_v3_queue_details_get,
        root_folder_api::api_v3_rootfolder_get,
    },
    models::{
        AddMovieOptions, MonitorTypes, MovieResource, MovieStatusType, QualityProfileResource,
        QueueResource, QueueStatus, RootFolderResource, TrackedDownloadState,
        TrackedDownloadStatus,
    },
};
use tracing::{debug, error, info, trace, warn};

/// Helper function to log detailed error information from Radarr API responses
fn log_api_error<T: std::fmt::Debug>(err: &RadarrApiError<T>, context: &str) {
    match err {
        RadarrApiError::ResponseError(response) => {
            super::api_logging::log_api_error_details(response.status, &response.content, context);
            if let Some(ref entity) = response.entity {
                debug!("Parsed error entity: {:#?}", entity);
            }
        }
        RadarrApiError::Reqwest(e) => {
            error!("{} - Reqwest error: {}", context, e);
        }
        RadarrApiError::Serde(e) => {
            error!("{} - Serialization error: {}", context, e);
        }
        RadarrApiError::Io(e) => {
            error!("{} - IO error: {}", context, e);
        }
    }
}

/// Treat a 2xx response whose body fails to parse as success - by the time we're
/// reading the body, Radarr has already applied the change
fn tolerate_response_parse_error<T, E>(
    result: std::result::Result<T, RadarrApiError<E>>,
    context: &str,
) -> Result<Option<T>>
where
    E: std::fmt::Debug + Send + Sync + 'static,
{
    match result {
        Ok(x) => Ok(Some(x)),
        Err(RadarrApiError::Serde(e)) => {
            warn!(
                "{} - succeeded, but the response body failed to parse: {}",
                context, e
            );
            Ok(None)
        }
        Err(e) => {
            log_api_error(&e, context);
            Err(e.into())
        }
    }
}

#[derive(Debug, Clone)]
pub struct Radarr {
    config: Configuration,
    details: Details,
}

#[derive(Debug, Clone)]
// All the details we want to collect
pub struct Details {
    rootfolders: Vec<RootFolderResource>,
    quality_profiles: Vec<QualityProfileResource>,
    monitor: Vec<MonitorTypes>,
    minimum_availability: Vec<MovieStatusType>,
}

#[derive(Debug)]
// The final details needed to complete the request
pub struct SelectedDetails {
    pub rootfolder_path: String,
    pub quality_profile_id: i32,
    pub monitor: MonitorTypes,
    pub minimum_availability: MovieStatusType,
}

impl Radarr {
    /// Builds the Radarr connection and attempts to use it
    pub async fn new(
        base_path: String,
        key: String,
        monitor_type: Option<MonitorTypes>,
        quality_profile: Option<String>,
        rootfolder: Option<String>,
        minimum_availability: Option<MovieStatusType>,
        client: reqwest::Client,
    ) -> Result<Self> {
        // Log connection before moving base_path
        info!("Connecting to Radarr at {}", base_path);

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
        let mut rootfolders = api_v3_rootfolder_get(&config).await.inspect_err(|e| {
            log_api_error(e, "Failed to get root folders from Radarr");
        })?;
        trace!("Retrieved {} root folders", rootfolders.len());

        let mut quality_profiles = api_v3_qualityprofile_get(&config).await.inspect_err(|e| {
            log_api_error(e, "Failed to get quality profiles from Radarr");
        })?;
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

        let minimum_availability = if let Some(x) = minimum_availability {
            vec![x]
        } else {
            vec![
                MovieStatusType::Tba,
                MovieStatusType::Announced,
                MovieStatusType::InCinemas,
                MovieStatusType::Released,
                MovieStatusType::Deleted,
            ]
        };

        let monitor = if let Some(x) = monitor_type {
            vec![x]
        } else {
            vec![
                MonitorTypes::MovieAndCollection,
                MonitorTypes::MovieOnly,
                MonitorTypes::None,
            ]
        };

        // Build the details
        let details = Details {
            rootfolders,
            quality_profiles,
            monitor,
            minimum_availability,
        };

        Ok(Self { config, details })
    }

    pub async fn connect(backend: BackendConfig, client: reqwest::Client) -> Result<Self> {
        if let BackendConfig::Radarr {
            url,
            api_key,
            monitor_type,
            quality_profile,
            rootfolder,
            minimum_availability,
        } = backend
        {
            Self::new(
                url,
                api_key,
                monitor_type,
                quality_profile,
                rootfolder,
                minimum_availability,
                client,
            )
            .await
        } else {
            bail!("Configured backend not for Radarr");
        }
    }
}

/// Helper function to get to and from stringified references
fn deserialize_from_string<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(&format!("\"{}\"", s))
        .with_context(|| format!("Failed to deserialize enum variant: {}", s))
}

mod field_keys {
    pub const ROOT_FOLDER: &str = "radarr:root_folder";
    pub const MONITOR: &str = "radarr:monitor";
    pub const AVAILABILITY: &str = "radarr:availability";
    pub const QUALITY_PROFILE: &str = "radarr:quality_profile";
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

        let monitor_options = details
            .monitor
            .iter()
            .map(|x| {
                let title = match x {
                    MonitorTypes::MovieOnly => "Movie Only",
                    MonitorTypes::MovieAndCollection => "Movie and Collection",
                    MonitorTypes::None => "None",
                };

                DropdownOption {
                    title: title.to_string(),
                    description: None,
                    id: Some(SelectableId::String(x.to_string())),
                }
            })
            .collect();

        let monitor_details = RequestDetails {
            title: "Monitor".to_string(),
            options: monitor_options,
            metadata: Some(field_keys::MONITOR.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Dropdown,
            always_show: false,
        };

        let availability_options = details
            .minimum_availability
            .iter()
            .map(|x| {
                let title = match x {
                    MovieStatusType::Announced => "Announced",
                    MovieStatusType::InCinemas => "In Cinemas",
                    MovieStatusType::Released => "Released",
                    MovieStatusType::Tba => "To Be Announced",
                    MovieStatusType::Deleted => "Deleted",
                };
                DropdownOption {
                    title: title.to_string(),
                    description: None,
                    id: Some(SelectableId::String(x.to_string())),
                }
            })
            .collect();

        let availability_details = RequestDetails {
            title: "Minimum Availability".to_string(),
            options: availability_options,
            metadata: Some(field_keys::AVAILABILITY.to_string()),
            selected_indices: vec![],
            field_type: FieldType::Dropdown,
            always_show: false,
        };

        vec![
            rootfolder_details,
            monitor_details,
            availability_details,
            quality_profile_details,
        ]
    }
}

impl TryFrom<Vec<RequestDetails>> for SelectedDetails {
    type Error = anyhow::Error;

    fn try_from(details: Vec<RequestDetails>) -> Result<Self> {
        let mut root_folder_path = None;
        let mut quality_profile_id = None;
        let mut monitor = None;
        let mut minimum_availability = None;

        for detail in &details {
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
                Some(field_keys::MONITOR) => {
                    monitor = match &selection.id {
                        Some(SelectableId::String(s)) => Some(deserialize_from_string(s)?),
                        other => bail!("Monitor must have a string ID, got {other:?}"),
                    };
                }
                Some(field_keys::AVAILABILITY) => {
                    minimum_availability = match &selection.id {
                        Some(SelectableId::String(s)) => Some(deserialize_from_string(s)?),
                        other => bail!("Availability must have a string ID, got {other:?}"),
                    };
                }
                other => bail!("Unknown metadata key: {other:?}"),
            }
        }

        Ok(Self {
            rootfolder_path: root_folder_path.context("No root folder was selected")?,
            quality_profile_id: quality_profile_id.context("No quality profile was selected")?,
            monitor: monitor.context("No monitor type was selected")?,
            minimum_availability: minimum_availability
                .context("No minimum availability was selected")?,
        })
    }
}

/// Describes what Radarr is currently doing about a movie it already has
///
/// Ordered from most to least specific: a file on disk settles the matter no
/// matter what else is going on, an active download is the next most useful
/// thing to report, and the remaining cases explain why nothing is happening yet.
fn describe_status(movie: &MovieResource, queue: &[QueueResource]) -> String {
    if movie.has_file.flatten().unwrap_or(false) {
        return match file_quality(movie) {
            Some(quality) => format!("Already available ({quality})"),
            None => "Already available".to_string(),
        };
    }

    if let Some(item) = pick_queue_item(queue) {
        return describe_queue_item(item);
    }

    // Nothing on disk and nothing downloading - the movie is only in the
    // library because someone added it, so say what it's waiting on
    if !movie.monitored.unwrap_or(false) {
        return "Already in Radarr, but not monitored - nothing will be downloaded".to_string();
    }

    // `is_available` is Radarr's own verdict on whether the movie has passed the
    // minimum availability it was added with. Assume it has when absent, since
    // "searching" is the less alarming thing to be wrong about
    if movie.is_available.unwrap_or(true) {
        "Waiting to be available - Radarr is searching for a release".to_string()
    } else {
        match expected_release(movie) {
            Some(date) => {
                format!("Waiting to be available - not released yet (expected {date})")
            }
            None => "Waiting to be available - not released yet".to_string(),
        }
    }
}

/// The quality name of the movie file currently on disk, e.g. "Bluray-1080p"
fn file_quality(movie: &MovieResource) -> Option<String> {
    movie
        .movie_file
        .as_ref()?
        .quality
        .as_ref()?
        .quality
        .as_ref()?
        .name
        .clone()
        .flatten()
}

/// Picks the queue record worth reporting on
///
/// Radarr keeps one record per grab, so a movie whose first release stalled can
/// have several. A record that needs attention is the one the user wants to hear
/// about; otherwise any of them describes the download equally well.
fn pick_queue_item(queue: &[QueueResource]) -> Option<&QueueResource> {
    queue
        .iter()
        .find(|item| needs_attention(item))
        .or_else(|| queue.first())
}

fn needs_attention(item: &QueueResource) -> bool {
    matches!(
        item.status,
        Some(QueueStatus::Warning | QueueStatus::Failed | QueueStatus::DownloadClientUnavailable)
    ) || matches!(
        item.tracked_download_status,
        Some(TrackedDownloadStatus::Warning | TrackedDownloadStatus::Error)
    )
}

fn describe_queue_item(item: &QueueResource) -> String {
    // Radarr explains its warnings and failures here, and the explanation is
    // usually the actionable half of the message ("stalled with no connections")
    let detail = item
        .error_message
        .as_ref()
        .and_then(|message| message.as_deref())
        .map(str::trim)
        .filter(|message| !message.is_empty());
    let with_detail = |headline: String| match detail {
        Some(detail) => format!("{headline} - {detail}"),
        None => headline,
    };

    // The import states describe a download that has already finished, so they
    // take precedence over whatever the transfer status still says
    match item.tracked_download_state {
        Some(TrackedDownloadState::Importing | TrackedDownloadState::ImportPending) => {
            return with_detail("Downloaded - importing now".to_string());
        }
        Some(TrackedDownloadState::ImportBlocked) => {
            return with_detail("Downloaded, but the import is blocked".to_string());
        }
        Some(TrackedDownloadState::Failed | TrackedDownloadState::FailedPending) => {
            return with_detail("Download failed".to_string());
        }
        _ => {}
    }

    if needs_attention(item) {
        return with_detail(format!("Download stalled{}", progress_suffix(item)));
    }

    match item.status {
        Some(QueueStatus::Paused) => {
            with_detail(format!("Download paused{}", progress_suffix(item)))
        }
        Some(QueueStatus::Queued) => with_detail("Queued for download".to_string()),
        Some(QueueStatus::Delay) => with_detail("Waiting on an indexer delay".to_string()),
        Some(QueueStatus::Completed) => {
            with_detail("Download complete - waiting to be imported".to_string())
        }
        // Anything else is a transfer in flight, including the unknown status
        // older download clients report while they're still working
        _ => {
            let mut message = match download_progress(item) {
                Some(percent) => format!("Downloading - {percent}%"),
                None => "Downloading".to_string(),
            };
            if let Some(timeleft) = item
                .timeleft
                .as_ref()
                .and_then(|timeleft| timeleft.as_deref())
                .filter(|timeleft| !timeleft.is_empty())
            {
                message.push_str(&format!(", {timeleft} remaining"));
            }
            with_detail(message)
        }
    }
}

/// " at 47%", or nothing when the download client hasn't reported sizes
fn progress_suffix(item: &QueueResource) -> String {
    match download_progress(item) {
        Some(percent) => format!(" at {percent}%"),
        None => String::new(),
    }
}

/// Percentage of the download that has completed
fn download_progress(item: &QueueResource) -> Option<u32> {
    let size = item.size?;
    let sizeleft = item.sizeleft?;
    if size <= 0.0 {
        return None;
    }
    Some(((size - sizeleft) / size * 100.0).clamp(0.0, 100.0).round() as u32)
}

/// The date the movie is expected to become available, as a plain `YYYY-MM-DD`
///
/// Radarr sends full timestamps; the time of day is noise for this message.
fn expected_release(movie: &MovieResource) -> Option<String> {
    [
        &movie.digital_release,
        &movie.physical_release,
        &movie.in_cinemas,
    ]
    .into_iter()
    .filter_map(|date| date.as_ref().and_then(|date| date.as_deref()))
    .find_map(|date| date.get(..10).map(str::to_string))
}

impl MediaItem for MovieResource {
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
impl MediaBackend for Radarr {
    async fn search(&self, term: &str) -> Result<Vec<Box<dyn MediaItem>>> {
        info!("Searching Radarr for movie: {}", term);
        let results = api_v3_movie_lookup_get(&self.config, Some(term))
            .await
            .inspect_err(|e| {
                log_api_error(e, "Failed to search Radarr");
            })?;
        debug!("Found {} movie results", results.len());
        Ok(results
            .into_iter()
            .map(|m| Box::new(m) as Box<dyn MediaItem>)
            .collect())
    }

    fn early_stop(&self, media: &dyn MediaItem) -> bool {
        media
            .as_any()
            .downcast_ref::<MovieResource>()
            .map(|m| m.id.is_some_and(|id| id > 0))
            .unwrap_or(false)
    }

    async fn early_stop_message(&self, media: &dyn MediaItem) -> String {
        let Some(media) = media.as_any().downcast_ref::<MovieResource>() else {
            error!("early_stop_message called with wrong media type for Radarr backend");
            return EARLY_STOP_MESSAGE.to_string();
        };

        // Guaranteed by `early_stop`, which is the only thing that gets us here
        let Some(id) = media.id.filter(|id| *id > 0) else {
            return EARLY_STOP_MESSAGE.to_string();
        };

        info!(
            movie_id = id,
            "Movie already in Radarr, checking its status"
        );

        // The lookup payload is metadata with a library id stapled on, so re-read
        // the library record for the fields that describe progress, alongside the
        // queue for anything currently downloading. Telling "searching for a
        // release" apart from "downloading" needs both, so if either call fails we
        // fall back to the generic message rather than report the wrong state.
        let (movie, queue) = tokio::join!(
            api_v3_movie_id_get(&self.config, id),
            api_v3_queue_details_get(&self.config, Some(id), Some(false)),
        );

        let movie = match movie {
            Ok(movie) => movie,
            Err(e) => {
                log_api_error(&e, "Failed to get movie status from Radarr");
                return EARLY_STOP_MESSAGE.to_string();
            }
        };

        let queue = match queue {
            Ok(queue) => queue,
            Err(e) => {
                log_api_error(&e, "Failed to get queue status from Radarr");
                return EARLY_STOP_MESSAGE.to_string();
            }
        };

        trace!(movie = ?movie, queue = ?queue, "Status sources");
        let status = describe_status(&movie, &queue);
        debug!(
            movie_id = id,
            queued = queue.len(),
            status = %status,
            "Resolved movie status"
        );
        status
    }

    fn display_info(&self, media: &dyn MediaItem) -> MediaDisplayInfo {
        let Some(media) = media.as_any().downcast_ref::<MovieResource>() else {
            error!("display_info called with wrong media type for Radarr backend");
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

    async fn additional_details(&self, _media: &dyn MediaItem) -> Result<Vec<RequestDetails>> {
        Ok(self.details.clone().into())
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
            .downcast::<MovieResource>()
            .map_err(|_| anyhow::anyhow!("Invalid media type for Radarr"))?;

        // Update the media object with the selected options
        media.add_options = Some(Box::new(AddMovieOptions {
            monitor: Some(selected.monitor),
            search_for_movie: Some(true),
            ..Default::default()
        }));
        media.quality_profile_id = Some(selected.quality_profile_id);
        media.minimum_availability = Some(selected.minimum_availability);
        media.root_folder_path = Some(Some(selected.rootfolder_path.clone()));

        if selected.monitor != MonitorTypes::None {
            media.monitored = Some(true);
        }

        info!(
            "Requesting movie: {} (tmdb_id: {:?})",
            media.title.clone().flatten().unwrap_or_default(),
            media.tmdb_id
        );
        debug!(
            "Request details - rootfolder: {}, quality_profile_id: {}, monitor: {:?}, minimum_availability: {:?}",
            selected.rootfolder_path,
            selected.quality_profile_id,
            selected.monitor,
            selected.minimum_availability
        );
        trace!("Full media object: {:#?}", media);

        // Make the API call
        tolerate_response_parse_error(
            api_v3_movie_post(&self.config, Some(media)).await,
            "Failed to add movie to Radarr",
        )?;

        Ok(())
    }

    fn success_message(
        &self,
        _details: &[RequestDetails],
        media: &dyn MediaItem,
    ) -> SuccessMessage {
        let Some(media) = media.as_any().downcast_ref::<MovieResource>() else {
            error!("success_message called with wrong media type for Radarr backend");
            return SuccessMessage {
                summary: "Request submitted".into(),
                description: "Will be downloaded when available.".into(),
                thumbnail_url: None,
                embed_data: None,
            };
        };

        let title = media.title.clone().flatten().unwrap_or_default();
        let year = media.year.unwrap_or_default();
        let overview = media.overview.clone().flatten().unwrap_or_default();
        let genres: Vec<String> = media.genres.clone().flatten().unwrap_or_default();
        let external_url = media
            .tmdb_id
            .map(|id| format!("https://www.themoviedb.org/movie/{id}"));

        let embed_data = external_url.map(|external_url| EmbedData {
            title: format!("{title} ({year})"),
            media_type: "Movie",
            overview: truncate_for_embed(&overview),
            poster_url: media.remote_poster.clone().flatten().unwrap_or_default(),
            genres,
            runtime_minutes: media.runtime.map(|r| r as u32),
            studio_or_network: media.studio.clone().flatten(),
            director: None,
            external_url,
        });

        SuccessMessage {
            summary: format!("{title} ({year})"),
            description: "Will be downloaded when available.".to_string(),
            thumbnail_url: media.remote_poster.clone().flatten(),
            embed_data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radarr_api::models::{MovieFileResource, Quality, QualityModel};

    /// Build a single-select detail with one option, optionally pre-selected.
    fn detail(metadata: &str, title: &str, id: SelectableId, selected: bool) -> RequestDetails {
        RequestDetails {
            title: metadata.to_string(),
            options: vec![DropdownOption {
                title: title.to_string(),
                description: None,
                id: Some(id),
            }],
            selected_indices: if selected { vec![0] } else { vec![] },
            metadata: Some(metadata.to_string()),
            field_type: FieldType::Dropdown,
            always_show: false,
        }
    }

    /// A full set of details with every field explicitly selected by the user.
    fn full_details() -> Vec<RequestDetails> {
        vec![
            detail(
                field_keys::ROOT_FOLDER,
                "/movies",
                SelectableId::Integer(1),
                true,
            ),
            detail(
                field_keys::QUALITY_PROFILE,
                "HD",
                SelectableId::Integer(7),
                true,
            ),
            detail(
                field_keys::MONITOR,
                "Movie Only",
                SelectableId::String("movieOnly".into()),
                true,
            ),
            detail(
                field_keys::AVAILABILITY,
                "Released",
                SelectableId::String("released".into()),
                true,
            ),
        ]
    }

    #[test]
    fn try_from_all_selected() {
        let selected = SelectedDetails::try_from(full_details()).unwrap();
        assert_eq!(selected.rootfolder_path, "/movies");
        assert_eq!(selected.quality_profile_id, 7);
        assert_eq!(selected.monitor, MonitorTypes::MovieOnly);
        assert_eq!(selected.minimum_availability, MovieStatusType::Released);
    }

    #[test]
    fn try_from_preset_rootfolder_is_auto_selected() {
        // Admin preset collapses root folder to a single, hidden option that the
        // user never explicitly selects. The request must still succeed.
        let mut details = full_details();
        details[0].selected_indices = vec![];
        let selected = SelectedDetails::try_from(details).unwrap();
        assert_eq!(selected.rootfolder_path, "/movies");
    }

    #[test]
    fn try_from_unselected_multi_option_field_errors() {
        let mut details = full_details();
        // A genuine user-facing field with more than one option, left unselected.
        details[1].options.push(DropdownOption {
            title: "4K".into(),
            description: None,
            id: Some(SelectableId::Integer(8)),
        });
        details[1].selected_indices = vec![];
        assert!(SelectedDetails::try_from(details).is_err());
    }

    /// A movie in the library that nothing has downloaded yet.
    fn library_movie() -> MovieResource {
        MovieResource {
            id: Some(42),
            monitored: Some(true),
            has_file: Some(Some(false)),
            is_available: Some(true),
            ..MovieResource::new()
        }
    }

    /// A queue record for a download that is halfway through.
    fn queue_item() -> QueueResource {
        QueueResource {
            movie_id: Some(Some(42)),
            size: Some(1000.0),
            sizeleft: Some(530.0),
            status: Some(QueueStatus::Downloading),
            tracked_download_status: Some(TrackedDownloadStatus::Ok),
            tracked_download_state: Some(TrackedDownloadState::Downloading),
            ..QueueResource::new()
        }
    }

    fn movie_file(quality: &str) -> Box<MovieFileResource> {
        Box::new(MovieFileResource {
            quality: Some(Box::new(QualityModel {
                quality: Some(Box::new(Quality {
                    name: Some(Some(quality.to_string())),
                    ..Quality::new()
                })),
                ..QualityModel::new()
            })),
            ..MovieFileResource::new()
        })
    }

    #[test]
    fn status_reports_a_movie_on_disk_as_available() {
        let movie = MovieResource {
            has_file: Some(Some(true)),
            movie_file: Some(movie_file("Bluray-1080p")),
            ..library_movie()
        };
        assert_eq!(
            describe_status(&movie, &[]),
            "Already available (Bluray-1080p)"
        );
    }

    #[test]
    fn status_prefers_the_file_over_a_leftover_queue_record() {
        // An import that hasn't been cleared from the queue yet must not read as
        // "still downloading" once the file is on disk.
        let movie = MovieResource {
            has_file: Some(Some(true)),
            ..library_movie()
        };
        assert_eq!(
            describe_status(&movie, &[queue_item()]),
            "Already available"
        );
    }

    #[test]
    fn status_reports_download_progress() {
        let item = QueueResource {
            timeleft: Some(Some("00:12:34".to_string())),
            ..queue_item()
        };
        assert_eq!(
            describe_status(&library_movie(), &[item]),
            "Downloading - 47%, 00:12:34 remaining"
        );
    }

    #[test]
    fn status_reports_download_progress_without_an_estimate() {
        assert_eq!(
            describe_status(&library_movie(), &[queue_item()]),
            "Downloading - 47%"
        );
    }

    #[test]
    fn status_reports_a_stalled_download_with_radarrs_explanation() {
        let item = QueueResource {
            status: Some(QueueStatus::Warning),
            tracked_download_status: Some(TrackedDownloadStatus::Warning),
            error_message: Some(Some(
                "The download is stalled with no connections".to_string(),
            )),
            ..queue_item()
        };
        assert_eq!(
            describe_status(&library_movie(), &[item]),
            "Download stalled at 47% - The download is stalled with no connections"
        );
    }

    #[test]
    fn status_picks_the_queue_record_that_needs_attention() {
        // Radarr keeps a record per grab, so a healthy retry can sit alongside
        // the stalled release the user actually needs to hear about.
        let stalled = QueueResource {
            status: Some(QueueStatus::Warning),
            sizeleft: Some(1000.0),
            ..queue_item()
        };
        assert_eq!(
            describe_status(&library_movie(), &[queue_item(), stalled]),
            "Download stalled at 0%"
        );
    }

    #[test]
    fn status_reports_an_import_in_progress() {
        let item = QueueResource {
            status: Some(QueueStatus::Completed),
            tracked_download_state: Some(TrackedDownloadState::Importing),
            ..queue_item()
        };
        assert_eq!(
            describe_status(&library_movie(), &[item]),
            "Downloaded - importing now"
        );
    }

    #[test]
    fn status_reports_a_queued_download() {
        let item = QueueResource {
            status: Some(QueueStatus::Queued),
            sizeleft: Some(1000.0),
            ..queue_item()
        };
        assert_eq!(
            describe_status(&library_movie(), &[item]),
            "Queued for download"
        );
    }

    #[test]
    fn status_reports_a_monitored_movie_with_nothing_in_the_queue() {
        assert_eq!(
            describe_status(&library_movie(), &[]),
            "Waiting to be available - Radarr is searching for a release"
        );
    }

    #[test]
    fn status_reports_an_unreleased_movie_with_its_expected_date() {
        let movie = MovieResource {
            is_available: Some(false),
            digital_release: Some(Some("2026-09-01T00:00:00Z".to_string())),
            ..library_movie()
        };
        assert_eq!(
            describe_status(&movie, &[]),
            "Waiting to be available - not released yet (expected 2026-09-01)"
        );
    }

    #[test]
    fn status_reports_an_unmonitored_movie() {
        let movie = MovieResource {
            monitored: Some(false),
            ..library_movie()
        };
        assert_eq!(
            describe_status(&movie, &[]),
            "Already in Radarr, but not monitored - nothing will be downloaded"
        );
    }

    #[test]
    fn download_progress_ignores_a_client_that_reports_no_size() {
        let item = QueueResource {
            size: Some(0.0),
            ..queue_item()
        };
        assert_eq!(describe_status(&library_movie(), &[item]), "Downloading");
    }
}
