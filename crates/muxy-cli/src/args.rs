use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;

use muxy_client::{SshTarget, Start, UdpRelay, UdpSettings};
use muxy_protocol::MAX_PAIRING_HOSTS;

use crate::manage::help;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Command {
    Help,
    /// One command's usage, which needs no server.
    Usage(&'static str),
    Version,
    BuildInfo,
    Interactive,
    Manage(Box<crate::manage::args::Invocation>),
    Mobile(Mobile),
    /// Joins stdin and stdout to the server, for clients on other computers.
    /// With UDP, it also offers a port to connect to instead of ssh.
    Stdio(Start, Option<UdpRelay>),
    Open(PathBuf),
    /// Drives a browser tab in the desktop app on this computer.
    Browser(Box<crate::browser::Invocation>),
    /// Installs the agent skill into agent folders and these folders.
    InstallSkills(Vec<PathBuf>),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Mobile {
    Status,
    Enable { port: Option<u16> },
    Disable,
    Pair { addresses: Vec<String> },
    Revoke { device: String },
}

/// Reads the command, and the other computer whose server it uses when it
/// starts with `--host DESTINATION`. Help, version and build info describe
/// this computer's Atur Terminal, a bridge must never reach on to another computer,
/// and browser tabs live in this computer's desktop app, so none of them
/// takes `--host`.
pub(crate) fn parse(arguments: &[OsString]) -> io::Result<(Option<SshTarget>, Command)> {
    match arguments {
        [flag, destination, rest @ ..] if flag == "--host" => {
            if rest.first().is_some_and(|word| word == "--host") {
                return Err(invalid("duplicate option --host"));
            }
            let mut host: SshTarget = destination
                .to_str()
                .ok_or_else(|| invalid("--host must be UTF-8"))?
                .parse()?;
            let (rest, udp) = match rest {
                [flag, rest @ ..] if flag == "--udp" => (rest, true),
                _ => (rest, false),
            };
            if udp {
                host = host.with_udp(UdpSettings::default().from_environment());
            }
            match (command(rest)?, rest) {
                (
                    Command::Help
                    | Command::Version
                    | Command::BuildInfo
                    | Command::Stdio(..)
                    | Command::Open(_)
                    | Command::Browser(_)
                    | Command::InstallSkills(_),
                    [word, ..],
                ) => Err(invalid(&format!(
                    "--host can't be combined with {}",
                    word.to_string_lossy()
                ))),
                (command, _) => Ok((Some(host), command)),
            }
        }
        [flag] if flag == "--host" => Err(invalid(
            "usage: atur-term --host DESTINATION [--udp] [COMMAND]",
        )),
        _ => Ok((None, command(arguments)?)),
    }
}

fn command(arguments: &[OsString]) -> io::Result<Command> {
    match arguments {
        [flag] if flag == "--help" || flag == "-h" => return Ok(Command::Help),
        [flag] if flag == "--version" || flag == "-V" => return Ok(Command::Version),
        [flag] if flag == "--build-info" => return Ok(Command::BuildInfo),
        _ => {}
    }
    match arguments {
        [] => Ok(Command::Interactive),
        [command, .., flag] if command == "mobile" && (flag == "--help" || flag == "-h") => {
            Ok(Command::Usage(help::MOBILE))
        }
        [command, rest @ ..] if command == "mobile" => mobile(rest).map(Command::Mobile),
        [command, rest @ ..] if command == "stdio" => {
            stdio(rest).map(|(start, udp)| Command::Stdio(start, udp))
        }
        [command, .., flag]
            if command == "install-skills" && (flag == "--help" || flag == "-h") =>
        {
            Ok(Command::Usage(help::INSTALL_SKILLS))
        }
        [command, rest @ ..] if command == "install-skills" => {
            install_skills(rest).map(Command::InstallSkills)
        }
        [command, rest @ ..] if command == "browser" => crate::browser::parse(rest),
        [command] if command == "help" => Ok(Command::Help),
        [command, topic] if command == "help" => topic
            .to_str()
            .and_then(help::topic)
            .map(Command::Usage)
            .ok_or_else(|| invalid("unknown command; run atur-term --help")),
        [word] if names_folder(word) => Ok(Command::Open(word.into())),
        _ => {
            crate::manage::args::parse(arguments).map(|command| Command::Manage(Box::new(command)))
        }
    }
}

fn names_folder(word: &OsStr) -> bool {
    !word.is_empty()
        && !word.as_encoded_bytes().starts_with(b"-")
        && word.to_str().is_none_or(|word| help::topic(word).is_none())
}

fn mobile(arguments: &[OsString]) -> io::Result<Mobile> {
    let text: Vec<_> = arguments.iter().map(|argument| argument.to_str()).collect();
    match text.as_slice() {
        [] => Ok(Mobile::Status),
        [Some("enable")] => Ok(Mobile::Enable { port: None }),
        [Some("enable"), Some("--port"), Some(port)] => Ok(Mobile::Enable {
            port: Some(
                port.parse()
                    .map_err(|_| invalid("port must be a number from 1024 to 65535"))?,
            ),
        }),
        [Some("disable")] => Ok(Mobile::Disable),
        [Some("pair"), options @ ..] => pair(options),
        [Some("revoke"), Some(device)] if !device.is_empty() => Ok(Mobile::Revoke {
            device: (*device).to_owned(),
        }),
        _ => Err(invalid(
            "unknown mobile command; run atur-term mobile --help",
        )),
    }
}

fn pair(options: &[Option<&str>]) -> io::Result<Mobile> {
    let mut addresses = Vec::new();
    for option in options.chunks(2) {
        let [Some("--address"), Some(address)] = option else {
            return Err(invalid("usage: atur-term mobile pair [--address HOST]..."));
        };
        let address = (*address).to_owned();
        if muxy_protocol::validate_pairing_hosts(std::slice::from_ref(&address)).is_err() {
            return Err(invalid(&format!(
                "--address {address}: use a DNS name or an IPv4 address, without a port"
            )));
        }
        addresses.push(address);
    }
    if addresses.len() > MAX_PAIRING_HOSTS {
        return Err(invalid(&format!(
            "a pairing code holds at most {MAX_PAIRING_HOSTS} addresses"
        )));
    }
    Ok(Mobile::Pair { addresses })
}

fn install_skills(arguments: &[OsString]) -> io::Result<Vec<PathBuf>> {
    arguments
        .chunks(2)
        .map(|option| match option {
            [flag, directory] if flag == "--dir" && !directory.is_empty() => {
                Ok(PathBuf::from(directory))
            }
            _ => Err(invalid("usage: atur-term install-skills [--dir DIR]...")),
        })
        .collect()
}

const STDIO_USAGE: &str =
    "usage: atur-term stdio [--no-start] [--udp [--udp-ports LOW-HIGH] [--udp-idle SECONDS]]";

fn stdio(arguments: &[OsString]) -> io::Result<(Start, Option<UdpRelay>)> {
    let mut start = Start::IfNeeded;
    let mut udp: Option<UdpRelay> = None;
    let (mut ports, mut idle) = (None, None);
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        let mut value = |name: &str| {
            words
                .next()
                .and_then(|value| value.to_str())
                .ok_or_else(|| invalid(&format!("{name} needs a value; {STDIO_USAGE}")))
        };
        match word.to_str() {
            Some("--no-start") => start = Start::Never,
            Some("--udp") => udp = Some(UdpRelay::default()),
            Some("--udp-ports") => {
                ports = Some(
                    muxy_client::parse_ports(value("--udp-ports")?)
                        .ok_or_else(|| invalid("--udp-ports takes LOW-HIGH, both from 1024 up"))?,
                );
            }
            Some("--udp-idle") => {
                let seconds: u64 = value("--udp-idle")?
                    .parse()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| invalid("--udp-idle takes a number of seconds"))?;
                idle = Some(std::time::Duration::from_secs(seconds));
            }
            _ => return Err(invalid(STDIO_USAGE)),
        }
    }
    match (&mut udp, ports, idle) {
        (Some(relay), ports, idle) => {
            if let Some(ports) = ports {
                relay.ports = ports;
            }
            if let Some(idle) = idle {
                relay.idle = idle;
            }
        }
        (None, None, None) => {}
        (None, ..) => return Err(invalid(STDIO_USAGE)),
    }
    Ok((start, udp))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> io::Result<(Option<SshTarget>, Command)> {
        parse(&words.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn install_skills_takes_folders_but_no_host() -> io::Result<()> {
        assert_eq!(
            words(&["install-skills"])?.1,
            Command::InstallSkills(Vec::new())
        );
        assert_eq!(
            words(&["install-skills", "--dir", "a", "--dir", "b"])?.1,
            Command::InstallSkills(vec!["a".into(), "b".into()])
        );
        assert_eq!(
            words(&["install-skills", "--help"])?.1,
            Command::Usage(help::INSTALL_SKILLS)
        );
        for args in [
            vec!["install-skills", "--dir"],
            vec!["install-skills", "a"],
            vec!["--host", "example", "install-skills"],
        ] {
            assert!(words(&args).is_err(), "{args:?}");
        }
        Ok(())
    }

    #[test]
    fn stdio_takes_udp_options_and_nothing_else() -> io::Result<()> {
        let (_, command) = words(&["stdio"])?;
        assert_eq!(command, Command::Stdio(Start::IfNeeded, None));
        let (_, command) = words(&["stdio", "--no-start", "--udp"])?;
        assert_eq!(
            command,
            Command::Stdio(Start::Never, Some(UdpRelay::default()))
        );
        let (_, command) = words(&[
            "stdio",
            "--udp",
            "--udp-ports",
            "7000-7100",
            "--udp-idle",
            "90",
        ])?;
        assert_eq!(
            command,
            Command::Stdio(
                Start::IfNeeded,
                Some(UdpRelay {
                    ports: 7000..=7100,
                    idle: std::time::Duration::from_secs(90),
                })
            )
        );
        for args in [
            vec!["stdio", "--udp-ports", "7000-7100"],
            vec!["stdio", "--udp", "--udp-ports", "80-90"],
            vec!["stdio", "--udp", "--udp-idle", "0"],
            vec!["stdio", "--udp", "--udp-idle"],
            vec!["stdio", "--bogus"],
            vec!["--host", "box", "stdio", "--udp"],
        ] {
            assert!(words(&args).is_err(), "{args:?}");
        }
        Ok(())
    }

    #[test]
    fn host_takes_udp_before_the_command() -> io::Result<()> {
        let (host, command) = words(&["--host", "box", "--udp", "server", "status"])?;
        assert!(host.is_some_and(|host| host.wants_udp()));
        assert!(matches!(command, Command::Manage(_)));
        let (host, _) = words(&["--host", "box", "server", "status"])?;
        assert!(host.is_some());
        Ok(())
    }

    #[test]
    fn browser_commands_stay_on_this_computer_and_have_help() -> io::Result<()> {
        assert!(matches!(
            words(&["browser", "list"])?.1,
            Command::Browser(_)
        ));
        assert_eq!(words(&["browser"])?.1, Command::Usage(help::BROWSER));
        assert_eq!(
            words(&["help", "browser"])?.1,
            Command::Usage(help::BROWSER)
        );
        assert_eq!(words(&["help"])?.1, Command::Help);
        assert!(!names_folder(OsStr::new("browser")));
        for args in [
            vec!["--host", "example", "browser", "list"],
            vec!["help", "nothing"],
        ] {
            assert!(words(&args).is_err(), "{args:?}");
        }
        Ok(())
    }
}
