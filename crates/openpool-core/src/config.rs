//! Typed process configuration used by the thin API and worker binaries.

use std::{env, net::SocketAddr};

use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub environment: Environment,
    pub api_bind_addr: SocketAddr,
    pub worker_bind_addr: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Environment {
    Development,
    Test,
    Staging,
    Production,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_values(
            required("APP_ENV")?,
            required("API_BIND_ADDR")?,
            required("WORKER_BIND_ADDR")?,
        )
    }

    pub fn from_values(
        environment: impl AsRef<str>,
        api_bind_addr: impl AsRef<str>,
        worker_bind_addr: impl AsRef<str>,
    ) -> Result<Self, ConfigError> {
        let environment = match environment.as_ref() {
            "development" => Environment::Development,
            "test" => Environment::Test,
            "staging" => Environment::Staging,
            "production" => Environment::Production,
            value => return Err(ConfigError::InvalidEnvironment(value.to_owned())),
        };
        let api_bind_addr = api_bind_addr
            .as_ref()
            .parse()
            .map_err(|_| ConfigError::InvalidSocketAddress("API_BIND_ADDR"))?;
        let worker_bind_addr = worker_bind_addr
            .as_ref()
            .parse()
            .map_err(|_| ConfigError::InvalidSocketAddress("WORKER_BIND_ADDR"))?;
        Ok(Self {
            environment,
            api_bind_addr,
            worker_bind_addr,
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    env::var(name).map_err(|_| ConfigError::Missing(name))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("required environment variable {0} is missing")]
    Missing(&'static str),
    #[error("APP_ENV must be development, test, staging, or production; got {0}")]
    InvalidEnvironment(String),
    #[error("{0} must be a valid socket address")]
    InvalidSocketAddress(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_config() {
        let config =
            AppConfig::from_values("development", "127.0.0.1:8080", "127.0.0.1:8081").unwrap();
        assert_eq!(config.environment, Environment::Development);
    }

    #[test]
    fn rejects_unknown_environment() {
        assert!(matches!(
            AppConfig::from_values("local", "127.0.0.1:8080", "127.0.0.1:8081"),
            Err(ConfigError::InvalidEnvironment(_))
        ));
    }
}
