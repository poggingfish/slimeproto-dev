use std::{net::IpAddr, path::PathBuf};

use axum::http::StatusCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::token::TokenError;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub database_map_size_bytes: usize,
    pub registration_keys: Option<Vec<String>>,
    pub max_storage_per_user_bytes: Option<u64>,
    pub max_requests_per_minute_per_ip: Option<u64>,
    pub max_requests_per_minute_per_user: Option<u64>,
    pub trusted_proxy_ips: Vec<IpAddr>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_map_size_bytes: 1024 * 1024 * 1024,
            registration_keys: None,
            max_storage_per_user_bytes: Some(100 * 1024 * 1024),
            max_requests_per_minute_per_ip: Some(600),
            max_requests_per_minute_per_user: Some(300),
            trusted_proxy_ips: Vec::new(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let explicit = std::env::var_os("SLIMEPROTO_CONFIG");
        let path = explicit
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("slimeproto.json"));
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let config: Self = serde_json::from_slice(&bytes).map_err(|_| "invalid server config: expected database_map_size_bytes, registration_keys, max_storage_per_user_bytes, max_requests_per_minute_per_ip, max_requests_per_minute_per_user, and/or trusted_proxy_ips with valid types")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), &'static str> {
        const MIB: usize = 1024 * 1024;
        if self.database_map_size_bytes < MIB || !self.database_map_size_bytes.is_multiple_of(MIB) {
            return Err("database_map_size_bytes must be a positive multiple of 1048576 bytes");
        }
        Ok(())
    }

    pub fn check_registration(&self, supplied: Option<&str>) -> Result<(), TokenError> {
        match &self.registration_keys {
            None => Ok(()),
            Some(keys) => {
                let valid = supplied.is_some_and(|supplied| {
                    let digest = Sha256::digest(supplied.as_bytes());
                    keys.iter()
                        .any(|key| Sha256::digest(key.as_bytes()) == digest)
                });
                if valid {
                    Ok(())
                } else {
                    Err(TokenError::Request(
                        StatusCode::FORBIDDEN,
                        "registration requires a valid registration key".into(),
                    ))
                }
            }
        }
    }

    pub fn registration_body_limit(&self) -> usize {
        self.registration_keys
            .as_ref()
            .into_iter()
            .flatten()
            .map(|key| key.len().saturating_mul(6).saturating_add(16 * 1024))
            .max()
            .unwrap_or(16 * 1024)
    }
}
