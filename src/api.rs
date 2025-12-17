use std::env;

use reqwest::{Client, RequestBuilder};

use crate::error::{Error, Result};

fn required_env(name: &'static str) -> Result<String> {
    env::var(name).map_err(|e| match e {
        env::VarError::NotPresent => Error::MissingEnvVar(name),
        _ => Error::InvalidEnvVar { name, source: e },
    })
}

#[derive(Debug, Clone)]
pub struct Config {
    pub username: String,
    pub password: String,
    pub tenant: String,
}

impl Config {
    pub fn new(username: String, password: String, tenant: String) -> Self {
        Self {
            username,
            password,
            tenant,
        }
    }

    pub fn from_env() -> Result<Self> {
        Ok(Self {
            username: required_env("ANTITHESIS_USERNAME")?,
            password: required_env("ANTITHESIS_PASSWORD")?,
            tenant: required_env("ANTITHESIS_TENANT")?,
        })
    }
}

pub struct AntithesisApi {
    client: Client,
    base_url: String,
    username: String,
    password: String,
}

impl AntithesisApi {
    pub fn new(config: Config) -> Result<Self> {
        let base_url = format!("https://{}.antithesis.com/api/v1", config.tenant);
        Self::with_base_url(config, base_url)
    }

    pub fn from_env() -> Result<Self> {
        let config = Config::from_env()?;
        Self::new(config)
    }

    pub fn with_base_url(config: Config, base_url: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username: config.username,
            password: config.password,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn get(&self, path: &str) -> RequestBuilder {
        self.client
            .get(format!("{}{}", self.base_url, path))
            .basic_auth(&self.username, Some(&self.password))
    }

    pub fn post(&self, path: &str) -> RequestBuilder {
        self.client
            .post(format!("{}{}", self.base_url, path))
            .basic_auth(&self.username, Some(&self.password))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{basic_auth, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn config_new_creates_config() {
        let config = Config::new("user".to_string(), "pass".to_string(), "tenant".to_string());
        assert_eq!(config.username, "user");
        assert_eq!(config.password, "pass");
        assert_eq!(config.tenant, "tenant");
    }

    #[tokio::test]
    async fn api_uses_basic_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/test"))
            .and(basic_auth("user", "pass"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = Config::new("user".to_string(), "pass".to_string(), "tenant".to_string());
        let api = AntithesisApi::with_base_url(config, mock_server.uri()).unwrap();

        let response = api.get("/test").send().await.unwrap();

        assert_eq!(response.status(), 200);
    }

    #[test]
    fn with_base_url_trims_trailing_slash() {
        let config = Config::new("user".to_string(), "pass".to_string(), "tenant".to_string());
        let api = AntithesisApi::with_base_url(config, "http://example.com/").unwrap();
        assert_eq!(api.base_url(), "http://example.com");
    }
}
