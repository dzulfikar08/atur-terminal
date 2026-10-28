#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
use linux::{agent_provider, foreground_member, process_directory};
#[cfg(target_os = "macos")]
use macos::{agent_provider, foreground_member, process_directory};

use super::backing::Backing;
use std::time::{Duration, Instant};

use muxy_protocol::{AgentProvider, ForegroundProcess, MetadataEvent, ServerPath};
use muxy_terminal::TerminalEvent;

/// Finding the agent walks every process on the computer, which costs about
/// 15 ms of the session thread. A foreground group that has just started may
/// still exec into an agent, so it is checked often at first.
const AGENT_SCAN_STARTING: Duration = Duration::from_millis(500);
const AGENT_STARTING: Duration = Duration::from_secs(5);
/// Later checks only catch a program that changes in place. A different
/// foreground group is always checked at once.
const AGENT_SCAN_SETTLED: Duration = Duration::from_secs(5);

fn agent_scan_interval(agent: Option<AgentProvider>, group_age: Duration) -> Duration {
    if agent.is_none() && group_age < AGENT_STARTING {
        AGENT_SCAN_STARTING
    } else {
        AGENT_SCAN_SETTLED
    }
}

pub(super) struct Metadata {
    pub(super) title: String,
    pub(super) directory: ServerPath,
    pub(super) process: Option<ForegroundProcess>,
    pub(super) agent: Option<AgentProvider>,
    next_agent: Instant,
    agent_group: Option<u32>,
    group_since: Instant,
    observed_directory: Option<ServerPath>,
}

impl Metadata {
    pub(super) fn new(directory: ServerPath) -> Self {
        Self {
            title: String::new(),
            directory,
            process: None,
            agent: None,
            next_agent: Instant::now(),
            agent_group: None,
            group_since: Instant::now(),
            observed_directory: None,
        }
    }

    pub(super) fn update(
        &mut self,
        pty: &dyn Backing,
        terminal: Vec<TerminalEvent>,
    ) -> Vec<MetadataEvent> {
        let mut events = Vec::new();
        let previous_directory = self.directory.clone();
        let group = pty.foreground_pid();
        let member = group.and_then(foreground_member);
        let now = Instant::now();
        if self.agent_group != group {
            self.group_since = now;
        }
        if self.next_agent <= now || self.agent_group != group {
            self.agent_group = group;
            let session_shell = group == Some(pty.child_pid())
                && member.as_ref().is_some_and(|(_, name)| {
                    matches!(
                        name.trim_start_matches('-'),
                        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh"
                    )
                });
            self.agent = if pty.detects_agents() {
                pty.agent().map(|(provider, _)| provider)
            } else if session_shell {
                None
            } else {
                group.and_then(agent_provider)
            };
            // herdr's answer is a cached lookup, so it is read as often as a new group is.
            self.next_agent = now
                + if pty.detects_agents() {
                    AGENT_SCAN_STARTING
                } else {
                    agent_scan_interval(self.agent, now - self.group_since)
                };
        }
        if let Some(group) = group {
            let process = ForegroundProcess {
                name: member
                    .as_ref()
                    .map_or_else(String::new, |(_, name)| name.clone()),
                is_shell: group == pty.child_pid(),
            };
            if self.process.as_ref() != Some(&process) {
                events.push(MetadataEvent::ForegroundProcess {
                    name: process.name.clone(),
                    is_shell: process.is_shell,
                });
                self.process = Some(process);
            }
            if let Some(directory) = member.and_then(|(pid, _)| process_directory(pid))
                && self.observed_directory.as_ref() != Some(&directory)
            {
                self.directory.clone_from(&directory);
                self.observed_directory = Some(directory);
            }
        }
        for event in terminal {
            match event {
                TerminalEvent::Title(title) if title != self.title => {
                    self.title.clone_from(&title);
                    events.push(MetadataEvent::Title(title));
                }
                TerminalEvent::Directory(directory) => {
                    if let Some(directory) = terminal_directory(&directory) {
                        self.directory = directory;
                    }
                }
                TerminalEvent::ClipboardWrite(text) => {
                    events.push(MetadataEvent::ClipboardWrite(text));
                }
                TerminalEvent::Bell => events.push(MetadataEvent::Bell),
                TerminalEvent::Progress(_) | TerminalEvent::Title(_) => {}
            }
        }
        if let Some(title) = pty.title()
            && title != self.title
        {
            self.title.clone_from(&title);
            events.push(MetadataEvent::Title(title));
        }
        if self.directory != previous_directory {
            events.push(MetadataEvent::Directory(self.directory.clone()));
        }
        events
    }
}

fn terminal_directory(value: &str) -> Option<ServerPath> {
    if value.starts_with('/') {
        return (!value.contains('\0')).then(|| ServerPath(value.as_bytes().to_vec()));
    }
    let (_, path) = value.strip_prefix("file://")?.split_once('/')?;
    let mut bytes = path.bytes();
    let mut path = vec![b'/'];
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()?
        } else {
            byte
        };
        if byte == 0 {
            return None;
        }
        path.push(byte);
    }
    Some(ServerPath(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_group_is_scanned_rarely_unless_it_is_still_starting() {
        let second = Duration::from_secs(1);
        assert_eq!(
            agent_scan_interval(None, Duration::ZERO),
            AGENT_SCAN_STARTING
        );
        assert_eq!(agent_scan_interval(None, 4 * second), AGENT_SCAN_STARTING);
        assert_eq!(agent_scan_interval(None, 5 * second), AGENT_SCAN_SETTLED);
        assert_eq!(
            agent_scan_interval(Some(AgentProvider::OhMyPi), Duration::ZERO),
            AGENT_SCAN_SETTLED
        );
    }
}
