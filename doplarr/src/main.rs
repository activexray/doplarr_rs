use anyhow::bail;
use clap::Parser;
use config::{Backend, BackendConfig};
use discord::InteractionContinue;
use providers::{
    MediaBackend, UserFacingError, radarr::Radarr, seerr::Seerr as SeerrBackend, sonarr::Sonarr,
    sportarr::Sportarr,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tokio::{
    sync::{Mutex, mpsc},
    time::{Duration, interval},
};
use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::EnvFilter;
use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_gateway::{Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _};
use twilight_http::Client as HttpClient;
use twilight_model::application::interaction::{
    InteractionData, application_command::CommandOptionValue,
};

pub mod args;
pub mod config;
pub mod discord;
pub mod providers;

/// Sanitize error messages for Discord users while keeping full details in logs
fn user_facing_error(err: &anyhow::Error) -> String {
    if let Some(e) = err.downcast_ref::<UserFacingError>() {
        return e.0.clone();
    }

    let err_msg = err.to_string().to_lowercase();

    if err_msg.contains("timeout") || err_msg.contains("timed out") {
        "Request timed out. The backend server may be slow or unavailable."
    } else if err_msg.contains("connection") || err_msg.contains("connect") {
        "Could not connect to the backend server. Please try again later."
    } else if err_msg.contains("401")
        || err_msg.contains("403")
        || err_msg.contains("unauthorized")
        || err_msg.contains("forbidden")
    {
        "Backend authentication error. Please contact your administrator."
    } else if err_msg.contains("500") || err_msg.contains("502") || err_msg.contains("503") {
        "The backend server encountered an error. Please try again later."
    } else {
        "An error occurred while processing your request. Please try again or contact your administrator."
    }
    .to_string()
}

type InteractionMap = Arc<Mutex<HashMap<uuid::Uuid, (mpsc::Sender<InteractionContinue>, Instant)>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse command line args to get path to config file
    let cli = args::Cli::parse();

    // Load the config, generating one from environment variables or writing a
    // starter template if it doesn't exist yet
    let Some(config) = config::Config::load_or_init(cli.config_file.unwrap())? else {
        // A starter template was written; nothing to run until it's filled in
        return Ok(());
    };

    // Setup logging with configured level
    let log_level = config.log_level.as_deref().unwrap_or("info");
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        git_hash = env!("GIT_HASH"),
        "Starting doplarr"
    );

    // Check that we have at least one backend client
    if config.backends.is_empty() {
        bail!("At least one media backend is required!");
    }

    // Check that all media types are unique
    let mut media_types = HashSet::new();
    if !config
        .backends
        .iter()
        .all(|x| media_types.insert(x.media.as_str()))
    {
        bail!("There must only be one of each media type");
    }

    // Build the HTTP request client for backend calls with a reasonable timeout
    let backend_http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    // Connect to all available backends, cast into trait objects, and associate with their media types
    let mut backends = HashMap::new();
    for Backend { media, config } in &config.backends {
        let backend: Arc<dyn MediaBackend> = match config {
            BackendConfig::Radarr { .. } => {
                Arc::new(Radarr::connect(config.clone(), backend_http.clone()).await?)
            }
            BackendConfig::Sonarr { .. } => {
                Arc::new(Sonarr::connect(config.clone(), backend_http.clone()).await?)
            }
            BackendConfig::Seerr { .. } => {
                Arc::new(SeerrBackend::connect(config.clone(), backend_http.clone()).await?)
            }
            BackendConfig::Sportarr { .. } => {
                Arc::new(Sportarr::connect(config.clone(), backend_http.clone()).await?)
            }
        };
        backends.insert(media.as_str(), backend);
    }

    // We listen for interactions, plus guild events so we can register commands
    // for every guild as Discord announces it (including guilds joined while running)
    let mut shard = Shard::new(ShardId::ONE, config.discord_token.clone(), Intents::GUILDS);

    // Create the HTTP client we use to send data *back* to Discord
    let discord_http = Arc::new(HttpClient::new(config.discord_token));

    // Cache the application ID for repeated use later in the process.
    let application_id = {
        let response = discord_http.current_user_application().await?;
        response.model().await?.id
    };

    // Build the list of media types we'll register commands for
    info!("Available backends: {:?}", media_types);
    let command = discord::commands(media_types.iter().copied());

    // Cache interactions
    let cache = DefaultInMemoryCache::builder()
        .resource_types(ResourceType::INTEGRATION)
        .build();

    // Build our map that holds each interaction -> (sender, timestamp) for the particular event flow
    let in_progress_interactions: InteractionMap = Arc::new(Mutex::new(HashMap::new()));

    // Spawn a background task to clean up abandoned interactions
    const INTERACTION_TIMEOUT: Duration = Duration::from_secs(300);
    const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);
    {
        let in_progress = Arc::clone(&in_progress_interactions);
        tokio::spawn(async move {
            let mut ticker = interval(CLEANUP_INTERVAL);
            loop {
                ticker.tick().await;
                let mut map = in_progress.lock().await;
                let now = Instant::now();
                let before_count = map.len();

                map.retain(|uuid, (_tx, timestamp)| {
                    let age = now.duration_since(*timestamp);
                    if age > INTERACTION_TIMEOUT {
                        debug!(uuid = %uuid, age_secs = age.as_secs(), "Cleaning up abandoned interaction");
                        false
                    } else {
                        true
                    }
                });

                let removed = before_count - map.len();
                if removed > 0 {
                    info!("Cleaned up {} abandoned interaction(s)", removed);
                }
            }
        });
    }

    // Finally, process the stream of events as they come in
    while let Some(item) = shard
        .next_event(
            EventTypeFlags::READY
                | EventTypeFlags::GUILD_CREATE
                | EventTypeFlags::INTERACTION_CREATE,
        )
        .await
    {
        // Make sure we have a good event
        let Ok(event) = item else {
            error!(source = ?item.unwrap_err(), "Error receiving event");
            continue;
        };

        // Update the cache with the event.
        cache.update(&event);

        // Determine if we're starting an interaction flow or updating an existing one
        match event {
            Event::Ready(_) => {
                info!("Connected to Discord's server");
            }
            // Discord sends one of these per guild after READY, and again whenever
            // the bot joins a new guild, so this covers initial and runtime registration
            Event::GuildCreate(guild) => {
                let guild_id = guild.id();
                info!(guild_id = %guild_id, "Registering commands to guild");
                if let Err(e) = discord_http
                    .interaction(application_id)
                    .set_guild_commands(guild_id, std::slice::from_ref(&command))
                    .await
                {
                    error!(error = %e, guild_id = %guild_id, "Failed to register commands to guild");
                }
            }
            Event::InteractionCreate(interaction) => {
                trace!(data = ?interaction, "Got interaction event");
                match &interaction.data {
                    Some(InteractionData::ApplicationCommand(command_data)) => {
                        debug!(data = ?command_data, "Got application command");
                        // New interaction
                        // We now dispatch on the "name" of the interaction which selects the media kind, called with the query string
                        let (media_kind, query) = if command_data.name
                            == discord::TOP_LEVEL_COMMAND_NAME
                            && let Some(subcommand) = command_data.options.first()
                            && let CommandOptionValue::SubCommand(x) = &subcommand.value
                            && let Some(option) = x.first()
                            && option.name == discord::QUERY_COMMAND_NAME
                            && let CommandOptionValue::String(value) = &option.value
                        {
                            (subcommand.name.clone(), value.clone())
                        } else {
                            warn!(data = ?command_data, "Interaction body didn't match what we expected",);
                            continue;
                        };
                        info!(
                            kind = media_kind,
                            query = query,
                            user_id = ?interaction.author_id(),
                            guild_id = ?interaction.guild_id,
                            "Got search request"
                        );

                        // Create the channel that we'll push data through
                        let (tx, rx) = mpsc::channel(1);

                        // Add this channel to our map of in-progress interactions
                        let uuid = uuid::Uuid::new_v4();
                        in_progress_interactions
                            .lock()
                            .await
                            .insert(uuid, (tx, Instant::now()));

                        // Build the start data
                        let start = discord::InteractionStart {
                            uuid,
                            rx,
                            query,
                            media: media_kind.clone(),
                            interaction_id: interaction.id,
                            application_id,
                            token: interaction.token.clone(),
                            user_id: interaction
                                .author_id()
                                .expect("Interaction must have a user"),
                            channel_id: interaction
                                .channel
                                .as_ref()
                                .expect("Interaction must have a channel")
                                .id,
                        };

                        // Spawn the coroutine
                        tokio::spawn({
                            // Clone the HTTP clients so we can spawn the async task
                            let discord_http = Arc::clone(&discord_http);
                            let in_progress = Arc::clone(&in_progress_interactions);
                            let public_followup = config.public_followup.unwrap_or(true);
                            let backend = backends
                                .get(media_kind.as_str())
                                .expect("This will exist as we've checked earlier")
                                .clone();

                            async move {
                                // Keep token for error handling
                                let interaction_token = start.token.clone();

                                // Run the flow in its own task so a panic is contained here
                                // instead of silently killing the interaction
                                let result = match tokio::spawn(discord::run_interaction(
                                    start,
                                    discord_http.clone(),
                                    backend,
                                    public_followup,
                                ))
                                .await
                                {
                                    Ok(result) => result,
                                    Err(join_err) => Err(anyhow::anyhow!(
                                        "Interaction task panicked: {join_err}"
                                    )),
                                };

                                if let Err(e) = result {
                                    // A UserFacingError is an expected, user-actionable
                                    // outcome (e.g. seasons already monitored), not a
                                    // system failure - log it calmly. Everything else is
                                    // a real error worth an admin's attention.
                                    if e.downcast_ref::<UserFacingError>().is_some() {
                                        info!(uuid = %uuid, reason = %e, "Interaction ended with a user-facing message");
                                    } else {
                                        error!(uuid = %uuid, error = ?e, "Failed to run coroutine to completion");
                                    }

                                    // Show sanitized error to Discord user (no sensitive info)
                                    let user_msg = user_facing_error(&e);
                                    if let Err(update_err) = discord::update_string_message(
                                        &user_msg,
                                        &discord_http,
                                        application_id,
                                        &interaction_token,
                                    )
                                    .await
                                    {
                                        warn!(uuid = %uuid, error = %update_err, "Failed to send error message to user");
                                    }
                                }

                                // Clean up the interaction from the map
                                in_progress.lock().await.remove(&uuid);
                                debug!(uuid = %uuid, "Cleaned up completed interaction");
                            }
                        });
                    }
                    Some(InteractionData::MessageComponent(component_data)) => {
                        debug!(data=?component_data, "Got message component");
                        // This is a continuation of an interaction, send this update payload through the channel to the spawned coroutine
                        // Extract the UUID from the update message and push this new data into the associated channel to move that coroutine forward
                        if let Some((_, uuid)) = component_data.custom_id.split_once(':')
                            && let Ok(uuid) = uuid::Uuid::parse_str(uuid)
                        {
                            let tx = in_progress_interactions
                                .lock()
                                .await
                                .get(&uuid)
                                .map(|(tx, _)| tx.clone());
                            match tx {
                                Some(tx) => {
                                    // Build the continuation data
                                    let cont = InteractionContinue {
                                        data: component_data.clone(),
                                        interaction_id: interaction.id,
                                        token: interaction.token.clone(),
                                    };
                                    // Try to send, distinguishing "coroutine busy" from "coroutine gone"
                                    match tx.try_send(cont) {
                                        Ok(_) => {
                                            trace!("Sent continuation to interaction coroutine");
                                        }
                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                            // The coroutine is still processing the previous event
                                            // (e.g. the user is clicking quickly); drop this one
                                            debug!(uuid = %uuid, "Interaction coroutine busy, dropping extra event");
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => {
                                            // Other side timed out
                                            warn!(uuid = %uuid, "Interaction coroutine timed out");
                                            discord::update_timeout(
                                                &discord_http,
                                                application_id,
                                                &interaction.token,
                                            )
                                            .await.unwrap_or_else(|e| {
                                                warn!(error = %e, "Failed to update interaction with timeout message");
                                            });
                                            // Remove the TX from the map
                                            in_progress_interactions.lock().await.remove(&uuid);
                                            debug!(uuid = %uuid, "Removed timed out interaction from map");
                                        }
                                    }
                                }
                                None => {
                                    // User wanted to continue an interaction that we don't have an ID for, impling we cleaned it up from timeout
                                    // Alternatively, a user continued an interaction from a previous run of the bot, which means we don't have any interaction to update!
                                    warn!(uuid = %uuid, "No active interaction found for continuation");
                                    discord::update_timeout(
                                        &discord_http,
                                        application_id,
                                        &interaction.token,
                                    )
                                    .await.unwrap_or_else(|e| {
                                        warn!(error = %e, "Failed to update interaction with timeout message");
                                 });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::GatewayClose(_) => {} // Standard timeout, no need to log to debug or anything
            _ => debug!(event = ?event, "Got non-handled event"),
        }
    }
    Ok(())
}
