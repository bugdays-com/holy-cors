use clap::Parser;
use std::collections::HashSet;

/// Default allowed origins (bugdays.com)
const DEFAULT_ORIGINS: &[&str] = &[
    "https://bugdays.com",
    "https://www.bugdays.com",
    "http://bugdays.com",
    "http://www.bugdays.com",
];

/// Holy CORS! A fast CORS proxy for developers
#[derive(Parser, Debug, Clone)]
#[command(name = "holy-cors")]
#[command(author = "Bug Days")]
#[command(version)]
#[command(about = "Holy CORS! A fast CORS proxy for developers", long_about = None)]
pub struct Config {
    /// Port to listen on
    #[arg(short, long, default_value = "8080", env = "HOLY_CORS_PORT")]
    pub port: u16,

    /// Additional origins to allow (can be specified multiple times)
    #[arg(long = "allow-origin", env = "HOLY_CORS_ORIGINS", value_delimiter = ',')]
    pub allow_origins: Vec<String>,

    /// Allow all origins (development mode - be careful!)
    #[arg(long = "allow-all-origins", env = "HOLY_CORS_ALLOW_ALL", default_value = "false")]
    pub allow_all: bool,

    /// Enable verbose logging
    #[arg(short, long, env = "HOLY_CORS_VERBOSE", default_value = "false")]
    pub verbose: bool,

    /// Bind address (default: 0.0.0.0)
    #[arg(long, default_value = "0.0.0.0", env = "HOLY_CORS_BIND")]
    pub bind: String,
}

impl Config {
    /// Get all allowed origins as a HashSet for efficient lookup
    pub fn allowed_origins(&self) -> HashSet<String> {
        let mut origins: HashSet<String> = DEFAULT_ORIGINS.iter().map(|s| s.to_string()).collect();
        origins.extend(self.allow_origins.iter().cloned());
        origins
    }

    /// Check if an origin is allowed
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        if self.allow_all {
            return true;
        }
        self.allowed_origins().contains(origin)
    }

    /// Get the socket address to bind to
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.bind, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_origins() {
        let config = Config {
            port: 8080,
            allow_origins: vec![],
            allow_all: false,
            verbose: false,
            bind: "0.0.0.0".to_string(),
        };

        assert!(config.is_origin_allowed("https://bugdays.com"));
        assert!(config.is_origin_allowed("https://www.bugdays.com"));
        assert!(!config.is_origin_allowed("https://evil.com"));
    }

    #[test]
    fn test_custom_origin() {
        let config = Config {
            port: 8080,
            allow_origins: vec!["http://localhost:3000".to_string()],
            allow_all: false,
            verbose: false,
            bind: "0.0.0.0".to_string(),
        };

        assert!(config.is_origin_allowed("http://localhost:3000"));
        assert!(config.is_origin_allowed("https://bugdays.com"));
    }

    #[test]
    fn test_allow_all() {
        let config = Config {
            port: 8080,
            allow_origins: vec![],
            allow_all: true,
            verbose: false,
            bind: "0.0.0.0".to_string(),
        };

        assert!(config.is_origin_allowed("https://anything.com"));
        assert!(config.is_origin_allowed("http://localhost:9999"));
    }
}
