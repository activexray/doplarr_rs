#[derive(Debug, Clone)]
pub struct Configuration {
    /// Base url including any url base, without a trailing slash,
    /// e.g. `http://localhost:1867`
    pub base_path: String,
    pub user_agent: Option<String>,
    pub client: reqwest::Client,
    /// Sent as the `X-Api-Key` header on every request.
    pub api_key: Option<String>,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            base_path: "http://localhost:1867".to_owned(),
            user_agent: Some("sportarr_api/1.0.0/rust".to_owned()),
            client: reqwest::Client::new(),
            api_key: None,
        }
    }
}
