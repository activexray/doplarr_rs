use super::*;
use crate::{
    config::{BackendConfig, BookFormat, MediaKind},
    discord::MAX_DROPDOWN_OPTIONS,
};
use anyhow::{Result, bail};
use async_trait::async_trait;
use seerr_api::{
    apis::{
        Error as SeerrApiError,
        auth_api::auth_me_get,
        configuration::{ApiKey, Configuration},
        movies_api::movie_movie_id_get,
        request_api::request_post,
        search_api::search_get,
        tv_api::tv_tv_id_get,
        users_api::{user_get, user_user_id_settings_notifications_get},
    },
    models::{
        _request_post_request::MediaType, _search_get_200_response_results_inner::SearchResultId,
        RequestPostRequest, RequestPostRequestSeasons,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

fn log_api_error<T: std::fmt::Debug>(err: &SeerrApiError<T>, context: &str) {
    match err {
        SeerrApiError::ResponseError(response) => {
            super::api_logging::log_api_error_details(response.status, &response.content, context);
            if let Some(ref entity) = response.entity {
                debug!("Parsed error entity: {:#?}", entity);
            }
        }
        SeerrApiError::Reqwest(e) => error!("{} - Reqwest error: {}", context, e),
        SeerrApiError::Serde(e) => error!("{} - Serialization error: {}", context, e),
        SeerrApiError::Io(e) => error!("{} - IO error: {}", context, e),
    }
}

/// Log the API error details, then convert to anyhow for propagation.
const ENRICHED_KEY: &str = "seerr:enriched";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SeerrEnriched {
    genres: Vec<String>,
    runtime_minutes: Option<u32>,
    studio_or_network: Option<String>,
    director: Option<String>,
}

fn require<T, E>(result: std::result::Result<T, SeerrApiError<E>>, context: &str) -> Result<T>
where
    E: std::fmt::Debug + Send + Sync + 'static,
{
    result.map_err(|e| {
        log_api_error(&e, context);
        anyhow::Error::from(e)
    })
}

fn tolerate_response_parse_error<T, E>(
    result: std::result::Result<T, SeerrApiError<E>>,
    context: &str,
) -> Result<Option<T>>
where
    E: std::fmt::Debug + Send + Sync + 'static,
{
    match result {
        Ok(x) => Ok(Some(x)),
        Err(SeerrApiError::Serde(e)) => {
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

struct UserMapCache {
    map: HashMap<u64, i32>,
    fetched_at: Instant,
}

pub struct Seerr {
    config: Configuration,
    fallback_user_id: Option<i32>,
    allow_4k: bool,
    media_filter: Option<MediaKind>,
    book_format: Option<BookFormat>,
    allow_all_seasons: bool,
    user_cache: RwLock<Option<UserMapCache>>,
}

impl std::fmt::Debug for Seerr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seerr")
            .field("base_path", &self.config.base_path)
            .finish()
    }
}

impl Seerr {
    pub async fn connect(backend: BackendConfig, client: reqwest::Client) -> Result<Self> {
        let BackendConfig::Seerr {
            url,
            api_key,
            fallback_user_id,
            allow_4k,
            media_filter,
            book_format,
            allow_all_seasons,
        } = backend
        else {
            bail!("Expected Seerr config");
        };

        let base_path = format!("{}/api/v1", url.trim_end_matches('/'));
        let config = Configuration {
            base_path,
            client,
            api_key: Some(ApiKey {
                prefix: None,
                key: api_key,
            }),
            ..Default::default()
        };

        require(auth_me_get(&config).await, "Seerr auth validation")?;
        info!("Connected to Seerr at {}", config.base_path);

        Ok(Self {
            config,
            fallback_user_id,
            allow_4k: allow_4k.unwrap_or(false),
            media_filter,
            book_format,
            allow_all_seasons: allow_all_seasons.unwrap_or(true),
            user_cache: RwLock::new(None),
        })
    }

    async fn resolve_seerr_user(&self, discord_id: u64) -> Result<Option<i32>> {
        const DEBOUNCE: Duration = Duration::from_secs(30);

        {
            let cache = self.user_cache.read().await;
            if let Some(ref c) = *cache
                && c.fetched_at.elapsed() < DEBOUNCE
            {
                return Ok(c.map.get(&discord_id).copied());
            }
        }

        let mut cache = self.user_cache.write().await;
        // Re-check after acquiring write lock
        if let Some(ref c) = *cache
            && c.fetched_at.elapsed() < DEBOUNCE
        {
            return Ok(c.map.get(&discord_id).copied());
        }

        let mut map: HashMap<u64, i32> = HashMap::new();
        let mut skip = 0.0f64;
        loop {
            let page = require(
                user_get(
                    &self.config,
                    Some(100.0),
                    Some(skip),
                    None,
                    None,
                    None,
                    None,
                )
                .await,
                "Fetching Seerr users",
            )?;

            let users = page.results.unwrap_or_default();
            if users.is_empty() {
                break;
            }

            for user in &users {
                let notif = require(
                    user_user_id_settings_notifications_get(&self.config, user.id as f64).await,
                    "Fetching user notification settings",
                )?;

                if let Some(Some(ids)) = notif.discord_ids {
                    for id_str in ids {
                        if let Ok(did) = id_str.parse::<u64>() {
                            map.insert(did, user.id);
                        }
                    }
                }
            }

            let total = page.page_info.and_then(|p| p.results).unwrap_or(0.0) as usize;

            skip += users.len() as f64;
            if skip as usize >= total {
                break;
            }
        }

        info!(
            "Seerr user cache refreshed: {} linked Discord user(s)",
            map.len()
        );
        let result = map.get(&discord_id).copied();
        *cache = Some(UserMapCache {
            map,
            fetched_at: Instant::now(),
        });
        Ok(result)
    }
}

// The search result type is our MediaItem for Seerr
use seerr_api::models::SearchGet200ResponseResultsInner as SeerrResult;

fn result_title(result: &SeerrResult) -> &str {
    match result.media_type.as_str() {
        "tv" => result.name.as_deref().unwrap_or("Unknown"),
        _ => result.title.as_deref().unwrap_or("Unknown"),
    }
}

fn result_year(result: &SeerrResult) -> Option<String> {
    match result.media_type.as_str() {
        "tv" => result
            .first_air_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .map(str::to_string),
        "book" => result
            .first_publish_year
            .map(|year| (year as i32).to_string()),
        _ => result
            .release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .map(str::to_string),
    }
}

fn result_dropdown_id(result: &SeerrResult) -> Option<SelectableId> {
    match &result.id {
        SearchResultId::Number(id) => Some(SelectableId::Integer(*id as i32)),
        SearchResultId::String(id) => Some(SelectableId::String(id.clone())),
    }
}

fn result_poster_url(result: &SeerrResult) -> Option<String> {
    result.poster_path.as_ref().map(|path| {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else {
            format!("https://image.tmdb.org/t/p/w500{path}")
        }
    })
}

impl MediaItem for SeerrResult {
    fn to_dropdown(&self) -> DropdownOption {
        let display_name = result_title(self);
        let year = result_year(self);
        let type_tag = match self.media_type.as_str() {
            "movie" => "Movie",
            "tv" => "Series",
            "book" => "Book",
            _ => &self.media_type,
        };
        let description = match year.as_deref() {
            Some(y) => format!("{type_tag} · {y}"),
            None => type_tag.to_string(),
        };
        DropdownOption {
            title: display_name.to_string(),
            description: Some(description),
            id: result_dropdown_id(self),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

#[async_trait]
impl MediaBackend for Seerr {
    fn to_dropdown_options(&self, results: &[Box<dyn MediaItem>]) -> Vec<DropdownOption> {
        results
            .iter()
            .filter_map(|r| r.as_any().downcast_ref::<SeerrResult>())
            .map(|result| {
                let display_name = result_title(result);
                let year = result_year(result);
                let description = if self.media_filter.is_some() {
                    year
                } else {
                    let type_tag = match result.media_type.as_str() {
                        "movie" => "Movie",
                        "tv" => "Series",
                        "book" => "Book",
                        _ => &result.media_type,
                    };
                    Some(match year.as_deref() {
                        Some(y) => format!("{type_tag} · {y}"),
                        None => type_tag.to_string(),
                    })
                };
                DropdownOption {
                    title: display_name.to_string(),
                    description,
                    id: result_dropdown_id(result),
                }
            })
            .collect()
    }

    async fn search(&self, term: &str) -> Result<Vec<Box<dyn MediaItem>>> {
        let response = require(
            search_get(&self.config, term, None, None).await,
            "Seerr search",
        )?;

        let results = response
            .results
            .unwrap_or_default()
            .into_iter()
            .filter(|r| match &self.media_filter {
                Some(MediaKind::Movie) => r.media_type == "movie",
                Some(MediaKind::Tv) => r.media_type == "tv",
                Some(MediaKind::Book) => r.media_type == "book",
                None => r.media_type == "movie" || r.media_type == "tv",
            })
            .map(|r| Box::new(r) as Box<dyn MediaItem>)
            .collect();

        Ok(results)
    }

    fn early_stop(&self, media: &dyn MediaItem) -> bool {
        let Some(result) = media.as_any().downcast_ref::<SeerrResult>() else {
            return false;
        };
        let Some(ref info) = result.media_info else {
            return false;
        };
        match result.media_type.as_str() {
            "movie" => info
                .status
                .is_some_and(|status| (2.0..=5.0).contains(&status)),
            "tv" => info.status == Some(5.0),
            // Books are single-item requests, so pending, processing, partially
            // available, and available all mean another request would be a
            // duplicate. SeerrNG can leave media status UNKNOWN while an
            // approved request is queued, so also inspect its nested request
            // statuses (1 = pending approval, 2 = approved). This mirrors
            // movie handling; TV remains special because a user may still
            // request unrequested seasons.
            "book" => {
                info.status
                    .is_some_and(|status| (2.0..=5.0).contains(&status))
                    || info.requests.as_ref().is_some_and(|requests| {
                        requests
                            .iter()
                            .any(|request| matches!(request.status, 1.0 | 2.0))
                    })
            }
            _ => false,
        }
    }

    fn display_info(&self, media: &dyn MediaItem) -> MediaDisplayInfo {
        let Some(result) = media.as_any().downcast_ref::<SeerrResult>() else {
            return MediaDisplayInfo {
                title: "Unknown".into(),
                subtitle: None,
                description: None,
                thumbnail_url: None,
            };
        };
        let title = result_title(result).to_string();
        let year = result_year(result);
        let description = if result.media_type == "book" {
            result.author.as_ref().map(|author| format!("by {author}"))
        } else {
            result.overview.clone()
        };
        MediaDisplayInfo {
            title,
            subtitle: year,
            description,
            thumbnail_url: result_poster_url(result),
        }
    }

    async fn additional_details(&self, media: &dyn MediaItem) -> Result<Vec<RequestDetails>> {
        let Some(result) = media.as_any().downcast_ref::<SeerrResult>() else {
            return Ok(vec![]);
        };

        if result.media_type == "book" {
            let formats: Vec<BookFormat> = match self.book_format {
                Some(format) => vec![format],
                None => vec![BookFormat::Ebook, BookFormat::Audiobook, BookFormat::Both],
            };
            return Ok(vec![RequestDetails {
                title: "Format".into(),
                options: formats
                    .into_iter()
                    .map(|format| DropdownOption {
                        title: match format {
                            BookFormat::Ebook => "Ebook",
                            BookFormat::Audiobook => "Audiobook",
                            BookFormat::Both => "Ebook and Audiobook",
                        }
                        .into(),
                        description: None,
                        id: Some(SelectableId::String(format.as_str().into())),
                    })
                    .collect(),
                selected_indices: vec![],
                metadata: Some("seerr:book_format".into()),
                field_type: FieldType::Dropdown,
                always_show: self.book_format.is_none(),
            }]);
        }

        let quality_step = self.allow_4k.then(|| RequestDetails {
            title: "Quality".into(),
            options: vec![
                DropdownOption {
                    title: "Standard".into(),
                    description: None,
                    id: Some(SelectableId::Boolean(false)),
                },
                DropdownOption {
                    title: "4K".into(),
                    description: None,
                    id: Some(SelectableId::Boolean(true)),
                },
            ],
            selected_indices: vec![],
            metadata: Some("seerr:is_4k".into()),
            field_type: FieldType::Dropdown,
            always_show: true,
        });

        let mut opts: Vec<RequestDetails> = quality_step.into_iter().collect();

        let enriched = if result.media_type == "tv" {
            let tv_id = result
                .id
                .as_number()
                .ok_or_else(|| anyhow::anyhow!("TV search result has a non-numeric ID"))?;
            let details = require(
                tv_tv_id_get(&self.config, tv_id, None).await,
                "Fetching TV details",
            )?;

            let genres: Vec<String> = details
                .genres
                .unwrap_or_default()
                .into_iter()
                .filter_map(|g| g.name)
                .collect();
            let runtime_minutes = details
                .episode_run_time
                .as_ref()
                .and_then(|rts| rts.first().copied())
                .map(|r| r as u32);
            let studio_or_network = details
                .networks
                .unwrap_or_default()
                .into_iter()
                .next()
                .and_then(|n| n.name);
            let director = details
                .credits
                .as_ref()
                .and_then(|c| c.crew.as_ref())
                .and_then(|crew| {
                    crew.iter().find_map(|c| {
                        if c.job.as_deref() == Some("Director") {
                            c.name.clone()
                        } else {
                            None
                        }
                    })
                });

            let mut season_options: Vec<DropdownOption> = details
                .seasons
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.season_number.is_some_and(|n| n > 0.0))
                .map(|s| {
                    let n = s.season_number.unwrap() as i32;
                    DropdownOption {
                        title: n.to_string(),
                        description: None,
                        id: Some(SelectableId::Integer(n)),
                    }
                })
                .collect();

            if season_options.is_empty() {
                bail!(UserFacingError("No requestable seasons found.".into()));
            }

            let mut options: Vec<DropdownOption> = Vec::new();
            if self.allow_all_seasons {
                options.push(DropdownOption {
                    title: "All Seasons".into(),
                    description: Some("Includes future seasons".into()),
                    id: Some(SelectableId::Integer(ALL_SEASONS_ID)),
                });
            }
            let capacity = MAX_DROPDOWN_OPTIONS - options.len();
            if season_options.len() > capacity {
                warn!(
                    showing = capacity,
                    total = season_options.len(),
                    "Truncating season list to fit Discord dropdown limit"
                );
                season_options.truncate(capacity);
            }
            options.extend(season_options);

            opts.push(RequestDetails {
                title: "Season".into(),
                options,
                selected_indices: vec![],
                metadata: Some("seerr:season".into()),
                field_type: FieldType::MultiSelect,
                always_show: true,
            });

            SeerrEnriched {
                genres,
                runtime_minutes,
                studio_or_network,
                director,
            }
        } else {
            let movie_id = result
                .id
                .as_number()
                .ok_or_else(|| anyhow::anyhow!("Movie search result has a non-numeric ID"))?;
            let details = require(
                movie_movie_id_get(&self.config, movie_id, None).await,
                "Fetching movie details",
            )?;

            let director = details
                .credits
                .as_ref()
                .and_then(|c| c.crew.as_ref())
                .and_then(|crew| {
                    crew.iter().find_map(|c| {
                        if c.job.as_deref() == Some("Director") {
                            c.name.clone()
                        } else {
                            None
                        }
                    })
                });

            SeerrEnriched {
                genres: details
                    .genres
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|g| g.name)
                    .collect(),
                runtime_minutes: details.runtime.map(|r| r as u32),
                studio_or_network: details
                    .production_companies
                    .unwrap_or_default()
                    .into_iter()
                    .next()
                    .and_then(|p| p.name),
                director,
            }
        };

        let enriched_json = serde_json::to_string(&enriched).unwrap_or_default();
        opts.push(RequestDetails {
            title: String::new(),
            options: vec![],
            selected_indices: vec![],
            metadata: Some(ENRICHED_KEY.into()),
            field_type: FieldType::Dropdown,
            always_show: false,
        });
        // Hack: stash the JSON in the title field so RequestDetails isn't shown to the user
        opts.last_mut().unwrap().title = enriched_json;

        Ok(opts)
    }

    async fn request(
        &self,
        details: Vec<RequestDetails>,
        media: Box<dyn MediaItem>,
        requester_discord_id: u64,
    ) -> Result<()> {
        let result = media
            .into_any()
            .downcast::<SeerrResult>()
            .map_err(|_| anyhow::anyhow!("Unexpected media type for Seerr backend"))?;

        let seerr_user_id = match self.resolve_seerr_user(requester_discord_id).await? {
            Some(id) => id,
            None => match self.fallback_user_id {
                Some(id) => id,
                None => bail!(UserFacingError(format!(
                    "Your Discord account (ID: {requester_discord_id}) is not linked to a Seerr account. \
                     To link it, go to your Seerr profile → Settings → Notifications → Discord and enter your Discord User ID."
                ))),
            },
        };

        let media_type = match result.media_type.as_str() {
            "movie" => MediaType::Movie,
            "tv" => MediaType::Tv,
            "book" => MediaType::Book,
            other => bail!("Unsupported Seerr media type: {other}"),
        };

        let is_4k = details
            .iter()
            .find(|d| d.metadata.as_deref() == Some("seerr:is_4k"))
            .and_then(|d| d.selected_option())
            .and_then(|o| match &o.id {
                Some(SelectableId::Boolean(v)) => Some(*v),
                _ => None,
            })
            .unwrap_or(false);

        let seasons = if media_type == MediaType::Tv {
            details
                .iter()
                .find(|d| d.metadata.as_deref() == Some("seerr:season"))
                .map(|d| {
                    let selected: Vec<i32> = d
                        .selected_indices
                        .iter()
                        .filter_map(|&i| d.options.get(i))
                        .filter_map(|o| match &o.id {
                            Some(SelectableId::Integer(n)) => Some(*n),
                            _ => None,
                        })
                        .collect();

                    // "All Seasons" - let Seerr expand it (and apply its own
                    // future-season handling) via the "all" sentinel.
                    if selected.contains(&ALL_SEASONS_ID) {
                        RequestPostRequestSeasons::String("all".into())
                    } else {
                        let mut nums: Vec<f64> = selected.into_iter().map(|n| n as f64).collect();
                        nums.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        RequestPostRequestSeasons::ArrayVecf64(nums)
                    }
                })
        } else {
            None
        };

        let mut req = match (&result.id, media_type) {
            (SearchResultId::Number(id), MediaType::Movie | MediaType::Tv) => {
                RequestPostRequest::new(media_type, *id)
            }
            (SearchResultId::String(id), MediaType::Book) => {
                RequestPostRequest::new(media_type, id.clone())
            }
            _ => bail!(
                "Seerr returned an incompatible ID for {}",
                result.media_type
            ),
        };
        if media_type == MediaType::Book {
            req.format = details
                .iter()
                .find(|detail| detail.metadata.as_deref() == Some("seerr:book_format"))
                .and_then(RequestDetails::selected_option)
                .and_then(|option| match &option.id {
                    Some(SelectableId::String(format)) => Some(format.clone()),
                    _ => None,
                });
            req.edition_id = result.edition_id.clone();
            req.isbn13 = result.isbn13.clone();
            req.author_id = result.author_id.clone();
        } else {
            req.is4k = Some(is_4k);
        }
        req.seasons = seasons.map(Box::new);

        tolerate_response_parse_error(
            request_post(&self.config, req, Some(seerr_user_id)).await,
            "Seerr request",
        )?;
        Ok(())
    }

    fn success_message(&self, details: &[RequestDetails], media: &dyn MediaItem) -> SuccessMessage {
        let Some(result) = media.as_any().downcast_ref::<SeerrResult>() else {
            return SuccessMessage {
                summary: "Request submitted".into(),
                description: "Your request has been submitted.".into(),
                thumbnail_url: None,
                embed_data: None,
            };
        };

        let title = result_title(result).to_string();
        let year = result_year(result);

        let season_suffix = details
            .iter()
            .find(|d| d.metadata.as_deref() == Some("seerr:season"))
            .map(|d| {
                let mut nums: Vec<i32> = d
                    .selected_indices
                    .iter()
                    .filter_map(|&i| d.options.get(i))
                    .filter_map(|o| match &o.id {
                        Some(SelectableId::Integer(n)) => Some(*n),
                        _ => None,
                    })
                    .collect();
                if nums.contains(&ALL_SEASONS_ID) {
                    return " (All Seasons)".to_string();
                }
                nums.sort();
                match nums.as_slice() {
                    [] => String::new(),
                    [n] => format!(" (Season {n})"),
                    _ => format!(
                        " (Seasons {})",
                        nums.iter()
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            })
            .unwrap_or_default();

        let base = match year {
            Some(y) => format!("{title} ({y})"),
            None => title.clone(),
        };
        let format_suffix = details
            .iter()
            .find(|detail| detail.metadata.as_deref() == Some("seerr:book_format"))
            .and_then(RequestDetails::selected_option)
            .map(|option| format!(" ({})", option.title))
            .unwrap_or_default();
        let summary = format!("{base}{season_suffix}{format_suffix}");

        let thumbnail_url = result_poster_url(result);

        let external_url = match result.media_type.as_str() {
            "book" => result.id.as_str().map_or_else(String::new, |id| {
                format!("https://openlibrary.org/works/{id}")
            }),
            media_type => {
                let slug = if media_type == "tv" { "tv" } else { "movie" };
                let id = result.id.as_number().unwrap_or_default() as i64;
                format!("https://www.themoviedb.org/{slug}/{id}")
            }
        };

        let enriched = details
            .iter()
            .find(|d| d.metadata.as_deref() == Some(ENRICHED_KEY))
            .and_then(|d| serde_json::from_str::<SeerrEnriched>(&d.title).ok());

        let is_book = result.media_type == "book";
        let embed_data = EmbedData {
            title: base.clone(),
            media_type: match result.media_type.as_str() {
                "tv" => "TV Series",
                "book" => "Book",
                _ => "Movie",
            },
            overview: if is_book {
                result
                    .author
                    .as_ref()
                    .map(|author| format!("by {author}"))
                    .unwrap_or_default()
            } else {
                truncate_for_embed(&result.overview.clone().unwrap_or_default())
            },
            poster_url: thumbnail_url.clone().unwrap_or_default(),
            genres: if is_book {
                Vec::new()
            } else {
                enriched.as_ref().map_or(Vec::new(), |e| e.genres.clone())
            },
            runtime_minutes: (!is_book)
                .then(|| enriched.as_ref().and_then(|e| e.runtime_minutes))
                .flatten(),
            studio_or_network: if is_book {
                None
            } else {
                enriched.as_ref().and_then(|e| e.studio_or_network.clone())
            },
            director: if is_book {
                None
            } else {
                enriched.as_ref().and_then(|e| e.director.clone())
            },
            external_url,
        };

        SuccessMessage {
            summary,
            description: "Your request has been submitted to Seerr.".into(),
            thumbnail_url,
            embed_data: Some(embed_data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book_result() -> SeerrResult {
        let mut result = SeerrResult::new(SearchResultId::String("OL45804W".into()), "book".into());
        result.title = Some("The Left Hand of Darkness".into());
        result.author = Some("Ursula K. Le Guin".into());
        result.first_publish_year = Some(1969.0);
        result.poster_path = Some("https://covers.openlibrary.org/b/id/1-L.jpg".into());
        result.isbn13 = Some("9780441478125".into());
        result.edition_id = Some("OL1M".into());
        result.author_id = Some("OL21879A".into());
        result
    }

    fn backend(book_format: Option<BookFormat>) -> Seerr {
        Seerr {
            config: Configuration::default(),
            fallback_user_id: Some(1),
            allow_4k: false,
            media_filter: Some(MediaKind::Book),
            book_format,
            allow_all_seasons: true,
            user_cache: RwLock::new(None),
        }
    }

    #[tokio::test]
    async fn book_requests_offer_all_formats_by_default() {
        let result = book_result();
        let details = backend(None).additional_details(&result).await.unwrap();

        assert_eq!(details.len(), 1);
        assert_eq!(details[0].title, "Format");
        assert!(details[0].always_show);
        assert_eq!(
            details[0]
                .options
                .iter()
                .map(|option| option.title.as_str())
                .collect::<Vec<_>>(),
            ["Ebook", "Audiobook", "Ebook and Audiobook"]
        );
    }

    #[tokio::test]
    async fn configured_audiobook_format_is_auto_selected() {
        let result = book_result();
        let details = backend(Some(BookFormat::Audiobook))
            .additional_details(&result)
            .await
            .unwrap();

        assert!(!details[0].always_show);
        assert_eq!(
            details[0]
                .selected_option()
                .map(|option| option.title.as_str()),
            Some("Audiobook")
        );
    }

    #[test]
    fn book_success_message_uses_open_library_metadata() {
        let result = book_result();
        let message = backend(None).success_message(&[], &result);
        let embed = message
            .embed_data
            .expect("book result should produce an embed");

        assert_eq!(message.summary, "The Left Hand of Darkness (1969)");
        assert_eq!(embed.media_type, "Book");
        assert_eq!(embed.overview, "by Ursula K. Le Guin");
        assert_eq!(embed.external_url, "https://openlibrary.org/works/OL45804W");
        assert_eq!(
            message.thumbnail_url.as_deref(),
            Some("https://covers.openlibrary.org/b/id/1-L.jpg")
        );
    }

    #[test]
    fn pending_or_processing_books_stop_before_duplicate_request() {
        let backend = backend(Some(BookFormat::Audiobook));

        for status in [2.0, 3.0, 4.0, 5.0] {
            let mut result = book_result();
            let mut info = seerr_api::models::MediaInfo::new();
            info.status = Some(status);
            result.media_info = Some(Box::new(info));
            assert!(backend.early_stop(&result), "status {status} should stop");
        }

        for status in [1.0, 6.0] {
            let mut result = book_result();
            let mut info = seerr_api::models::MediaInfo::new();
            info.status = Some(status);
            result.media_info = Some(Box::new(info));
            assert!(
                !backend.early_stop(&result),
                "status {status} should allow a request"
            );
        }
    }

    #[test]
    fn active_nested_book_request_stops_when_media_status_is_unknown() {
        let backend = backend(Some(BookFormat::Audiobook));

        for request_status in [1.0, 2.0] {
            let mut result = book_result();
            let mut info = seerr_api::models::MediaInfo::new();
            info.status = Some(1.0);
            info.requests = Some(vec![seerr_api::models::MediaRequest::new(
                98.0,
                request_status,
            )]);
            result.media_info = Some(Box::new(info));
            assert!(
                backend.early_stop(&result),
                "request status {request_status} should stop"
            );
        }

        let mut declined = book_result();
        let mut info = seerr_api::models::MediaInfo::new();
        info.status = Some(1.0);
        info.requests = Some(vec![seerr_api::models::MediaRequest::new(98.0, 3.0)]);
        declined.media_info = Some(Box::new(info));
        assert!(!backend.early_stop(&declined));
    }
}
