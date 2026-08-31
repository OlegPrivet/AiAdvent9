use std::env;

use thiserror::Error;

const API_KEY_ENV: &str = "NEURALDEEP_API_KEY";
const DEFAULT_BASE_URL: &str = "https://api.neuraldeep.ru/v1";
const DEFAULT_MODEL: &str = "qwen3.8-27b";
const DEFAULT_MAX_TOKENS: u32 = 500;

#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) max_tokens: u32,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ConfigError {
    #[error("переменная окружения {API_KEY_ENV} не задана или пуста")]
    MissingApiKey,
}

impl Config {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| env::var(name).ok())
    }

    fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        let api_key = lookup(API_KEY_ENV)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or(ConfigError::MissingApiKey)?;

        Ok(Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_api_key() {
        let config = Config::from_lookup(|name| {
            assert_eq!(name, API_KEY_ENV);
            Some("  secret-key  ".to_owned())
        })
        .expect("valid key should be loaded");

        assert_eq!(config.api_key, "secret-key");
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.model, DEFAULT_MODEL);
        assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn rejects_missing_api_key() {
        assert_eq!(
            Config::from_lookup(|_| None).unwrap_err(),
            ConfigError::MissingApiKey
        );
    }

    #[test]
    fn rejects_empty_api_key() {
        assert_eq!(
            Config::from_lookup(|_| Some("   ".to_owned())).unwrap_err(),
            ConfigError::MissingApiKey
        );
    }
}
