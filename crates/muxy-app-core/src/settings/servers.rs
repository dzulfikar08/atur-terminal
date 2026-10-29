use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

use super::{Error, Result, Settings};
use crate::ServerId;

/// Another computer whose Atur Terminal server this app reaches over SSH.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerEntry {
    /// Any UUID spelling is read, such as `uuidgen` output.
    #[serde(deserialize_with = "any_uuid")]
    pub id: ServerId,
    pub name: String,
    /// An SSH destination: a `~/.ssh/config` alias, `user@host`, or
    /// `ssh://user@host:port`. It is fully checked when connecting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_command: Option<String>,
    /// The private key to log in with, instead of ssh's own choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    /// Logs in with a password, which the app asks for and never saves.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub password_login: bool,
    /// Moves the connection onto encrypted UDP once ssh has logged in, which
    /// holds up on slow, lossy or changing networks. ssh carries it when UDP
    /// does not connect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub udp: bool,
}

impl ServerEntry {
    pub fn new(name: String, ssh: String) -> Self {
        Self {
            id: ServerId::new(),
            name,
            ssh,
            ssh_command: None,
            identity_file: None,
            password_login: false,
            udp: false,
        }
    }

    pub fn destination(&self) -> &str {
        self.ssh_command.as_deref().unwrap_or(&self.ssh)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.is_local() {
            return Err(Error::new(
                "servers",
                "this computer's server ID cannot be listed",
            ));
        }
        if self.name.trim().is_empty()
            || self.name.len() > 256
            || self.name.chars().any(char::is_control)
        {
            return Err(Error::new(
                "servers",
                "enter a name of 1–256 bytes without control characters",
            ));
        }
        if let Some(command) = &self.ssh_command {
            if !self.ssh.is_empty() || self.identity_file.is_some() || self.password_login {
                return Err(Error::new(
                    "servers",
                    "custom commands cannot be combined with an SSH destination, identity file, or password login",
                ));
            }
            muxy_core::executable::command_arguments(command)
                .map_err(|error| Error::new("servers", error))?;
        } else if self.ssh.trim().is_empty() || self.ssh.chars().any(char::is_control) {
            return Err(Error::new(
                "servers",
                "enter an SSH destination without control characters",
            ));
        }
        if self
            .identity_file
            .as_ref()
            .is_some_and(|path| path.trim().is_empty() || path.chars().any(char::is_control))
        {
            return Err(Error::new(
                "servers",
                "enter an identity file path without control characters",
            ));
        }
        Ok(())
    }
}

impl Settings {
    pub fn server(&self, id: ServerId) -> Option<&ServerEntry> {
        self.servers.iter().find(|server| server.id == id)
    }

    pub fn add_server(&mut self, server: ServerEntry, path: &Path) -> Result<()> {
        self.servers = edit_servers(path, |servers| {
            servers.push(server);
            Ok(())
        })?;
        Ok(())
    }

    /// Replaces the entry that has `server`'s ID.
    pub fn update_server(&mut self, server: ServerEntry, path: &Path) -> Result<()> {
        self.servers = edit_servers(path, |servers| {
            let entry = servers
                .iter_mut()
                .find(|entry| entry.id == server.id)
                .ok_or_else(|| Error::new("servers", "this server is no longer listed"))?;
            *entry = server;
            Ok(())
        })?;
        Ok(())
    }

    pub fn remove_server(&mut self, id: ServerId, path: &Path) -> Result<()> {
        self.servers = edit_servers(path, |servers| {
            servers.retain(|server| server.id != id);
            Ok(())
        })?;
        Ok(())
    }

    pub(super) fn validate_servers(&self) -> Result<()> {
        validate(&self.servers)
    }

    /// The `[[servers]]` of the settings file at `path`, read as they are:
    /// the file is neither created nor changed. Entries that cannot be read
    /// or that fail validation, and repeats of an ID, are left out.
    pub fn servers_in(path: &Path) -> Result<Vec<ServerEntry>> {
        let mut document = super::appearance::read_document(path)
            .map_err(|error| Error::new(path.display().to_string(), error))?;
        let Some(toml::Value::Array(entries)) = document.remove("servers") else {
            return Ok(Vec::new());
        };
        let mut servers: Vec<ServerEntry> = Vec::new();
        for entry in entries {
            if let Ok(entry) = entry.try_into::<ServerEntry>()
                && entry.validate().is_ok()
                && !servers.iter().any(|listed| listed.id == entry.id)
            {
                servers.push(entry);
            }
        }
        Ok(servers)
    }

    /// Appends `servers` to the list in the file at `path` in one write.
    /// Entries whose ID is already listed are ignored.
    pub fn add_servers(path: &Path, servers: Vec<ServerEntry>) -> Result<()> {
        edit_servers(path, |listed| {
            for server in servers {
                if !listed.iter().any(|entry| entry.id == server.id) {
                    listed.push(server);
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}

fn any_uuid<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<ServerId, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

fn validate(servers: &[ServerEntry]) -> Result<()> {
    let mut ids = HashSet::new();
    for server in servers {
        server.validate()?;
        if !ids.insert(server.id) {
            return Err(Error::new("servers", "server IDs must be unique"));
        }
    }
    Ok(())
}

/// Changes the list as the file holds it now, so entries edited by hand
/// since loading are kept. An empty list is left out, so `[[servers]]` can be
/// added by hand.
fn edit_servers(
    path: &Path,
    change: impl FnOnce(&mut Vec<ServerEntry>) -> Result<()>,
) -> Result<Vec<ServerEntry>> {
    let mut document =
        super::appearance::read_document(path).map_err(|error| Error::new("servers", error))?;
    let mut servers: Vec<ServerEntry> = document
        .remove("servers")
        .map(toml::Value::try_into)
        .transpose()
        .map_err(|error| Error::new("servers", error))?
        .unwrap_or_default();
    change(&mut servers)?;
    validate(&servers)?;
    if !servers.is_empty() {
        let value =
            toml::Value::try_from(&servers).map_err(|error| Error::new("servers", error))?;
        document.insert("servers".into(), value);
    }
    super::appearance::write_document(path, &document)
        .map_err(|error| Error::new("servers", error))?;
    Ok(servers)
}
