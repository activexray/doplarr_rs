use anyhow::Context;
use radarr_api::models::{MonitorTypes as RadarrMonitor, MovieStatusType};
use serde::{Deserialize, Serialize};
use sonarr_api::models::SeriesTypes;
use std::fs;

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub log_level: Option<String>,
    pub public_followup: Option<bool>,
    pub discord_token: String,
    pub backends: Vec<Backend>,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone)]
pub struct Backend {
    pub media: String,
    pub config: BackendConfig,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Movie,
    Tv,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Eq, Clone)]
/// All of the backend-specific configuration, passed to the backend constructors
pub enum BackendConfig {
    Radarr {
        url: String,
        api_key: String,
        monitor_type: Option<RadarrMonitor>,
        quality_profile: Option<String>,
        rootfolder: Option<String>,
        minimum_availability: Option<MovieStatusType>,
    },
    Sonarr {
        url: String,
        api_key: String,
        quality_profile: Option<String>,
        rootfolder: Option<String>,
        series_type: Option<SeriesTypes>,
        season_folders: Option<bool>,
        /// Offer Season 0 (specials) in the season picker (default: false)
        allow_specials: Option<bool>,
        /// Offer an "All Seasons" option that monitors all current and future
        /// seasons (default: true)
        allow_all_seasons: Option<bool>,
    },
    Seerr {
        url: String,
        /// Must be an admin API key (generated in Seerr under Settings → API Key)
        api_key: String,
        /// Attribute requests from unlinked Discord users to this Seerr user ID; if absent, unlinked users are rejected.
        /// Users link by setting their Discord User ID in Seerr: Profile → Settings → Notifications → Discord
        fallback_user_id: Option<i32>,
        /// Present the "4K" quality option to users. Defaults to false.
        allow_4k: Option<bool>,
        /// Restrict search results to a single media kind.
        /// When absent, both movies and TV shows are returned.
        media_filter: Option<MediaKind>,
        /// Offer an "All Seasons" option in the season picker (default: true)
        allow_all_seasons: Option<bool>,
    },
    Sportarr {
        url: String,
        api_key: String,
        /// Pin every request to this quality profile by name; when absent, the
        /// requester picks from a dropdown
        quality_profile: Option<String>,
    },
}

/// Starter config written when no config file exists and no migration
/// environment variables are detected.
const TEMPLATE: &str = r#"# Doplarr configuration
#
# Any value can be pulled from an environment variable with ${VAR}, which is
# handy for secrets, e.g.  api_key = "${SEERR_API_KEY}"
#
# Fill in your Discord token and uncomment at least one backend below.

discord_token = "your_discord_bot_token"

# --- Seerr (Overseerr / Jellyseerr) ---
# [[backends]]
# media = "media"
#
# [backends.config.Seerr]
# url = "http://localhost:5055"
# api_key = "${SEERR_API_KEY}"

# --- Sonarr ---
# [[backends]]
# media = "series"
#
# [backends.config.Sonarr]
# url = "http://localhost:8989"
# api_key = "${SONARR_API_KEY}"

# --- Radarr ---
# [[backends]]
# media = "movie"
#
# [backends.config.Radarr]
# url = "http://localhost:7878"
# api_key = "${RADARR_API_KEY}"

# --- Sportarr ---
# [[backends]]
# media = "sport"
#
# [backends.config.Sportarr]
# url = "http://localhost:1867"
# api_key = "${SPORTARR_API_KEY}"
"#;

/// Expand `${VAR}` references against the process environment. Expansion
/// happens everywhere except inside `#` comments (so documenting the syntax in a
/// comment is inert); it does apply inside quoted strings. An unset variable,
/// a variable set to the empty string, or an unterminated `${` is a hard error.
/// Single-line strings only.
fn expand_env_vars(input: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_basic = false; // inside "..."
    let mut in_literal = false; // inside '...'
    let mut in_comment = false;

    while let Some(c) = chars.next() {
        if in_comment {
            out.push(c);
            if c == '\n' {
                in_comment = false;
            }
            continue;
        }

        // `${VAR}` expands in normal text and inside strings, but not comments
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            let mut closed = false;
            for vc in chars.by_ref() {
                if vc == '}' {
                    closed = true;
                    break;
                }
                var.push(vc);
            }
            if !closed {
                anyhow::bail!("Unterminated `${{` in config (missing closing `}}`)");
            }
            let var = var.trim();
            let val = std::env::var(var).map_err(|_| {
                anyhow::anyhow!("Config references environment variable `{var}`, which is not set")
            })?;
            if val.is_empty() {
                anyhow::bail!(
                    "Config references environment variable `{var}`, which is set but empty"
                );
            }
            out.push_str(&val);
            continue;
        }

        if in_basic {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                }
                '"' => in_basic = false,
                _ => {}
            }
            continue;
        }

        if in_literal {
            out.push(c);
            if c == '\'' {
                in_literal = false;
            }
            continue;
        }

        match c {
            '"' => in_basic = true,
            '\'' => in_literal = true,
            '#' => in_comment = true,
            _ => {}
        }
        out.push(c);
    }
    Ok(out)
}

/// Build a config from legacy Doplarr (Clojure) environment variables, using
/// `is_set` to probe the environment. Returns `None` unless a Discord token and
/// at least one backend are present. Values are emitted as `${VAR}` references
/// so secrets never land on disk.
fn generate_from_env(is_set: impl Fn(&str) -> bool) -> Option<String> {
    if !is_set("DISCORD__TOKEN") {
        return None;
    }

    let mut backends = String::new();

    let seerr = is_set("OVERSEERR__URL") && is_set("OVERSEERR__API");
    let sonarr = is_set("SONARR__URL") && is_set("SONARR__API");
    let radarr = is_set("RADARR__URL") && is_set("RADARR__API");

    if seerr {
        // The legacy Clojure bot exposed Overseerr as separate movie and series
        // commands, so mirror that with two media-filtered Seerr backends rather
        // than one combined command. Overseerr fronts Sonarr/Radarr, so when it
        // is configured it owns both commands and the direct *arr backends are
        // skipped to avoid duplicate command names.
        if sonarr || radarr {
            eprintln!(
                "Note: OVERSEERR__* is set, so requests are routed through Seerr and the \
                 SONARR__*/RADARR__* variables are ignored. Remove the Overseerr variables \
                 (or edit the generated config) if you want to request from Sonarr/Radarr directly."
            );
        }

        let mut push_seerr = |media: &str, filter: &str| {
            backends.push_str(&format!(
                "\n[[backends]]\nmedia = \"{media}\"\n\n[backends.config.Seerr]\n\
                 url = \"${{OVERSEERR__URL}}\"\napi_key = \"${{OVERSEERR__API}}\"\n\
                 media_filter = \"{filter}\"\n"
            ));
            if is_set("OVERSEERR__DEFAULT_ID") {
                // Unquoted so the substituted value parses as an integer
                backends.push_str("fallback_user_id = ${OVERSEERR__DEFAULT_ID}\n");
            }
        };

        push_seerr("movie", "movie");
        push_seerr("series", "tv");
    } else {
        if sonarr {
            backends.push_str(
                "\n[[backends]]\nmedia = \"series\"\n\n[backends.config.Sonarr]\n\
                 url = \"${SONARR__URL}\"\napi_key = \"${SONARR__API}\"\n",
            );
        }

        if radarr {
            backends.push_str(
                "\n[[backends]]\nmedia = \"movie\"\n\n[backends.config.Radarr]\n\
                 url = \"${RADARR__URL}\"\napi_key = \"${RADARR__API}\"\n",
            );
        }
    }

    if backends.is_empty() {
        return None;
    }

    let mut config = String::from(
        "# Auto-generated by Doplarr from detected legacy environment variables.\n\
         # Values are read from the environment at runtime via ${VAR} substitution.\n\n\
         discord_token = \"${DISCORD__TOKEN}\"\n",
    );
    if is_set("LOG_LEVEL") {
        config.push_str("log_level = \"${LOG_LEVEL}\"\n");
    }
    config.push_str(&backends);

    Some(config)
}

impl Config {
    /// Parse a config from a TOML string, expanding `${VAR}` references first.
    fn from_toml_str(content: &str, source: &str) -> anyhow::Result<Self> {
        let expanded = expand_env_vars(content)
            .with_context(|| format!("Failed to expand environment variables in {source}"))?;
        toml::from_str(&expanded).with_context(|| format!("Failed to parse TOML in {source}"))
    }

    pub fn from_file(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        Self::from_toml_str(&content, &path.display().to_string())
    }

    /// Load the config at `path`. When it's missing, either generate one from
    /// detected legacy environment variables (so existing Clojure-style Docker
    /// deployments keep working with no volume), or write a starter template
    /// and return `None` so the caller can exit with guidance.
    pub fn load_or_init(path: impl AsRef<std::path::Path>) -> anyhow::Result<Option<Self>> {
        let path = path.as_ref();

        if path.exists() {
            return Self::from_file(path).map(Some);
        }

        if let Some(generated) =
            generate_from_env(|k| std::env::var_os(k).filter(|v| !v.is_empty()).is_some())
        {
            println!(
                "No config file at {}; generating one from detected Doplarr environment variables.",
                path.display()
            );
            // Best-effort persist (keeps ${VAR} references, no secrets on disk).
            // Running doesn't depend on the write succeeding.
            if let Err(e) = fs::write(path, &generated) {
                eprintln!("Warning: could not write {}: {e}", path.display());
            }
            return Self::from_toml_str(&generated, "generated config").map(Some);
        }

        fs::write(path, TEMPLATE)
            .with_context(|| format!("Failed to write starter config to {}", path.display()))?;
        println!(
            "No config file found. Wrote a starter config to {}.\n\
             Edit it (or set the DISCORD__TOKEN / OVERSEERR__* / SONARR__* / RADARR__* \
             environment variables) and restart.",
            path.display()
        );
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let config: Config = toml::from_str(
            r#"
           discord_token = "abc123"

           [[backends]]
           media = "movie"

           [backends.config.Radarr]
           url = "http://1.2.3.4:7878"
           api_key = "abc123"
           monitor_type = "movieOnly"
           rootfolder = "/storage/movies"
           minimum_availability= "announced"
        "#,
        )
        .unwrap();

        let expected = Config {
            discord_token: "abc123".to_string(),
            backends: vec![Backend {
                media: "movie".to_string(),
                config: BackendConfig::Radarr {
                    url: "http://1.2.3.4:7878".to_string(),
                    api_key: "abc123".to_string(),
                    monitor_type: Some(RadarrMonitor::MovieOnly),
                    rootfolder: Some("/storage/movies".to_string()),
                    minimum_availability: Some(MovieStatusType::Announced),
                    quality_profile: None,
                },
            }],
            log_level: None,
            public_followup: None,
        };

        assert_eq!(config, expected);
    }

    #[test]
    fn test_parse_seerr_config() {
        let config: Config = toml::from_str(
            r#"
           discord_token = "abc123"

           [[backends]]
           media = "media"

           [backends.config.Seerr]
           url = "http://1.2.3.4:5055"
           api_key = "abc123"
           fallback_user_id = 1
        "#,
        )
        .unwrap();

        let expected = Config {
            discord_token: "abc123".to_string(),
            backends: vec![Backend {
                media: "media".to_string(),
                config: BackendConfig::Seerr {
                    url: "http://1.2.3.4:5055".to_string(),
                    api_key: "abc123".to_string(),
                    fallback_user_id: Some(1),
                    allow_4k: None,
                    media_filter: None,
                    allow_all_seasons: None,
                },
            }],
            log_level: None,
            public_followup: None,
        };

        assert_eq!(config, expected);
    }

    #[test]
    fn expand_env_vars_substitutes_and_passes_through() {
        // PATH is reliably set in any environment we run tests in.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(
            expand_env_vars("a=${PATH};b").unwrap(),
            format!("a={path};b")
        );
        // No references: returned unchanged.
        assert_eq!(expand_env_vars("plain value").unwrap(), "plain value");
    }

    #[test]
    fn expand_env_vars_errors_on_unset_and_unterminated() {
        assert!(expand_env_vars("${DEFINITELY_NOT_SET_DOPLARR_VAR_XYZ}").is_err());
        assert!(expand_env_vars("oops ${UNTERMINATED").is_err());
    }

    #[test]
    fn expand_env_vars_errors_on_empty_string_var() {
        // Set a variable to the empty string, then verify it's rejected.
        let var = "DOPLARR_TEST_EMPTY_STRING";
        unsafe { std::env::set_var(var, "") };
        let result = expand_env_vars(&format!("${{{var}}}"));
        unsafe { std::env::remove_var(var) };
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains(var) && err.contains("empty"),
            "error should mention var and empty: {err}"
        );
    }

    #[test]
    fn expand_env_vars_ignores_comments_but_not_strings() {
        // A `${...}` in a comment is left untouched, even if the var is unset.
        let input = "# docs: api_key = \"${NOT_SET_VAR}\"\nkey = \"${PATH}\"";
        let path = std::env::var("PATH").unwrap();
        assert_eq!(
            expand_env_vars(input).unwrap(),
            format!("# docs: api_key = \"${{NOT_SET_VAR}}\"\nkey = \"{path}\"")
        );
    }

    #[test]
    fn generate_from_env_needs_token_and_a_backend() {
        // No token at all.
        assert!(generate_from_env(|_| false).is_none());
        // Token but no backend.
        assert!(generate_from_env(|k| k == "DISCORD__TOKEN").is_none());
        // Token + incomplete Seerr (url only).
        assert!(generate_from_env(|k| matches!(k, "DISCORD__TOKEN" | "OVERSEERR__URL")).is_none());
    }

    #[test]
    fn generate_from_env_splits_seerr_into_movie_and_series() {
        let set = [
            "DISCORD__TOKEN",
            "OVERSEERR__URL",
            "OVERSEERR__API",
            "OVERSEERR__DEFAULT_ID",
        ];
        let toml = generate_from_env(|k| set.contains(&k)).expect("should generate");

        assert!(toml.contains(r#"discord_token = "${DISCORD__TOKEN}""#));
        assert!(toml.contains(r#"url = "${OVERSEERR__URL}""#));
        // Two media-filtered Seerr backends, mirroring the Clojure movie/series split.
        assert!(toml.contains(r#"media = "movie""#));
        assert!(toml.contains(r#"media = "series""#));
        assert!(toml.contains(r#"media_filter = "movie""#));
        assert!(toml.contains(r#"media_filter = "tv""#));
        // fallback_user_id wired onto both backends.
        assert_eq!(
            toml.matches("fallback_user_id = ${OVERSEERR__DEFAULT_ID}")
                .count(),
            2
        );
        // No combined "media" command anymore.
        assert!(!toml.contains(r#"media = "media""#));

        // The whole generated config must round-trip once the env is substituted.
        let expanded = expand_env_vars_for_test(&toml);
        Config::from_toml_str(&expanded, "test").expect("generated config must parse");
    }

    #[test]
    fn generate_from_env_overseerr_takes_precedence_over_arr() {
        // Everything set: Overseerr fronts the *arrs, so direct Sonarr/Radarr
        // backends are skipped to avoid duplicate command names.
        let toml = generate_from_env(|_| true).expect("should generate");
        assert!(toml.contains("[backends.config.Seerr]"));
        assert!(!toml.contains("[backends.config.Sonarr]"));
        assert!(!toml.contains("[backends.config.Radarr]"));
    }

    #[test]
    fn generate_from_env_uses_direct_arr_without_overseerr() {
        let set = ["DISCORD__TOKEN", "SONARR__URL", "SONARR__API"];
        let toml = generate_from_env(|k| set.contains(&k)).expect("should generate");
        assert!(toml.contains("[backends.config.Sonarr]"));
        assert!(toml.contains(r#"media = "series""#));
        assert!(!toml.contains("Seerr"));
        assert!(!toml.contains("Radarr"));
    }

    /// Substitute the env vars that the generated configs reference so the
    /// result can be parsed in tests, without touching the real environment.
    fn expand_env_vars_for_test(toml: &str) -> String {
        let pairs = [
            ("${DISCORD__TOKEN}", "tok"),
            ("${OVERSEERR__URL}", "http://seerr:5055"),
            ("${OVERSEERR__API}", "key"),
            ("${OVERSEERR__DEFAULT_ID}", "1"),
            ("${SONARR__URL}", "http://sonarr:8989"),
            ("${SONARR__API}", "key"),
            ("${RADARR__URL}", "http://radarr:7878"),
            ("${RADARR__API}", "key"),
            ("${LOG_LEVEL}", "info"),
        ];
        let mut out = toml.to_string();
        for (from, to) in pairs {
            out = out.replace(from, to);
        }
        out
    }
}
