//! Traits that the various backends will implement to support search and requests
//!
//! A generic provider does four things:
//! 1. Perform searches
//! 2. Determines if a selected search result is already available or has been requested before
//! 3. Provides a set of additional information needed to complete the request (quality profile, season, etc)
//! 4. Perform the request using the payload and the set of additional information and respond with a success or failure
use crate::discord::EARLY_STOP_MESSAGE;
use anyhow::Result;
use async_trait::async_trait;
use std::{any::Any, fmt::Debug};

#[derive(Debug)]
pub struct UserFacingError(pub String);

impl std::fmt::Display for UserFacingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UserFacingError {}

// Shared utilities
mod api_logging;

// Backend instances
pub mod radarr;
pub mod seerr;
pub mod sonarr;

/// Sentinel id for an "All Seasons" entry in a season multi-select. Real season
/// numbers are >= 0, so -1 never collides. The Discord layer treats an option
/// with this id as mutually exclusive with the rest of the multi-select.
pub const ALL_SEASONS_ID: i32 = -1;

/// Represents the different ways we can capture a unique id for a menu selection
/// Some objects in the backends have unique integer ids, while some are just string sentinel values
#[derive(Debug, Clone)]
pub enum SelectableId {
    Integer(i32),
    String(String),
    Boolean(bool),
}

#[derive(Debug, Clone, Default)]
pub struct DropdownOption {
    /// Main dropdown description
    pub title: String,
    /// Subtitle in the dropdown
    pub description: Option<String>,
    /// Backend-specific id
    pub id: Option<SelectableId>,
}

/// Type of field for the request detail
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    /// Single-item enum/list selection
    Dropdown,
    /// Multiple-item selection; selected indices tracked in `RequestDetails::selected_indices`
    MultiSelect,
    /// Boolean yes/no selection
    Boolean,
}

#[derive(Debug, Clone)]
/// Additional details needed to complete a request
pub struct RequestDetails {
    /// Title to present to the user for this collection of options
    pub title: String,
    /// Options to select
    pub options: Vec<DropdownOption>,
    /// For `MultiSelect` fields: indices into `options` that the user has currently chosen.
    /// Always empty for other field types.
    pub selected_indices: Vec<usize>,
    /// Backend-specific metadata
    pub metadata: Option<String>,
    /// Type of field
    pub field_type: FieldType,
    /// Show this field even when only a single option remains - single-option
    /// fields are otherwise hidden, as they represent admin-configured defaults
    pub always_show: bool,
}

/// Represents the media selection box as presented by discord
pub struct MediaDisplayInfo {
    pub title: String,
    pub subtitle: Option<String>,
    pub description: Option<String>,
    pub thumbnail_url: Option<String>,
}

/// Represents the success block shown by discord
pub struct SuccessMessage {
    /// Short one-liner identifying what was requested, e.g. "Title (Year) (Season 2)"
    /// Used as the heading and as OS notification content
    pub summary: String,
    pub description: String,
    pub thumbnail_url: Option<String>,
    /// Rich metadata for the public embed. When `Some`, a rich embed is posted
    /// alongside the plain-text `summary` in the public follow-up message.
    pub embed_data: Option<EmbedData>,
}

/// Rich metadata for the public follow-up embed
#[derive(Debug, Clone)]
pub struct EmbedData {
    pub title: String,
    pub media_type: &'static str,
    pub overview: String,
    pub poster_url: String,
    pub genres: Vec<String>,
    pub runtime_minutes: Option<u32>,
    pub studio_or_network: Option<String>,
    pub director: Option<String>,
    pub external_url: String,
}

/// Truncate a string to Discord's embed description limit (4096 chars).
pub fn truncate_for_embed(s: &str) -> String {
    let limit = 4096;
    if s.len() <= limit {
        s.to_string()
    } else {
        let mut end = limit;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

impl RequestDetails {
    /// Returns the currently selected option (for single-select fields).
    ///
    /// A single-option, non-multi field is an admin-configured default (e.g. a
    /// preset `rootfolder`): it is hidden from the user and rendered as static
    /// text rather than a dropdown, so it never receives an explicit selection.
    /// We treat its sole option as selected, matching the UI which already
    /// considers such fields resolved.
    pub fn selected_option(&self) -> Option<&DropdownOption> {
        if let Some(&i) = self.selected_indices.first() {
            return self.options.get(i);
        }
        if self.field_type != FieldType::MultiSelect && self.options.len() == 1 {
            return self.options.first();
        }
        None
    }

    /// Returns all currently selected options (for multi-select fields).
    pub fn selected_options(&self) -> impl Iterator<Item = &DropdownOption> {
        self.selected_indices
            .iter()
            .filter_map(|&i| self.options.get(i))
    }
}

// Trait that all media types must implement
pub trait MediaItem: Send + Sync + Debug {
    fn to_dropdown(&self) -> DropdownOption;

    fn as_any(&self) -> &dyn Any;

    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

#[async_trait]
pub trait MediaBackend: Send + Sync {
    /// Given a search term, return a vector of things that can be converted into Discord's `SelectMenuOption`
    async fn search(&self, term: &str) -> Result<Vec<Box<dyn MediaItem>>>;

    /// Convert search results into dropdown options for display.
    /// Backends can override this to customize labels based on their own context
    /// (e.g. suppressing the media-kind tag when results are already filtered).
    fn to_dropdown_options(&self, results: &[Box<dyn MediaItem>]) -> Vec<DropdownOption> {
        results.iter().map(|x| x.to_dropdown()).collect()
    }

    /// Given a search results payload, determine if we should stop the interaction flow early
    /// Not all providers will be able to do this with the payload alone, but this needs to not require a backend request
    fn early_stop(&self, media: &dyn MediaItem) -> bool;

    /// The message shown to the user when [`MediaBackend::early_stop`] ends the flow
    ///
    /// This runs only once that decision has been made, so - unlike `early_stop`
    /// itself - a backend may query for the media's current status and report
    /// something more useful than "already requested". A backend that can't, or
    /// whose lookup fails, falls back to the generic message.
    async fn early_stop_message(&self, _media: &dyn MediaItem) -> String {
        EARLY_STOP_MESSAGE.to_string()
    }

    /// Return the media display info
    fn display_info(&self, media: &dyn MediaItem) -> MediaDisplayInfo;

    /// Return the additional details we want to collect in order to complete a request
    async fn additional_details(&self, media: &dyn MediaItem) -> Result<Vec<RequestDetails>>;

    /// Perform the request with the backend, using the information gathered
    /// from the media search result and the additional details
    async fn request(
        &self,
        details: Vec<RequestDetails>,
        media: Box<dyn MediaItem>,
        requester_discord_id: u64,
    ) -> Result<()>;

    /// Build the success message including details about what was requested
    fn success_message(&self, details: &[RequestDetails], media: &dyn MediaItem) -> SuccessMessage;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(field_type: FieldType, n_options: usize, selected: Vec<usize>) -> RequestDetails {
        RequestDetails {
            title: "Test".into(),
            options: (0..n_options)
                .map(|i| DropdownOption {
                    title: format!("opt{i}"),
                    description: None,
                    id: Some(SelectableId::Integer(i as i32)),
                })
                .collect(),
            selected_indices: selected,
            metadata: None,
            field_type,
            always_show: false,
        }
    }

    #[test]
    fn single_option_dropdown_auto_selects() {
        // Admin-configured default: one option, hidden from the user, never
        // explicitly selected. Must still resolve to that option.
        let d = detail(FieldType::Dropdown, 1, vec![]);
        assert_eq!(d.selected_option().map(|o| o.title.as_str()), Some("opt0"));
    }

    #[test]
    fn multi_option_dropdown_requires_explicit_selection() {
        let d = detail(FieldType::Dropdown, 3, vec![]);
        assert!(d.selected_option().is_none());

        let d = detail(FieldType::Dropdown, 3, vec![2]);
        assert_eq!(d.selected_option().map(|o| o.title.as_str()), Some("opt2"));
    }

    #[test]
    fn single_option_multiselect_does_not_auto_select() {
        // For multi-select, "nothing selected" is a meaningful, distinct state.
        let d = detail(FieldType::MultiSelect, 1, vec![]);
        assert!(d.selected_option().is_none());
    }
}
