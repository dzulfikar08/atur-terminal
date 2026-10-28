mod languages;
mod process;

use super::{Arc, AtomicBool, Duration, Ordering, Phase, PoisonError, Recorder, Shared};
use crate::tr;
use process::Process;
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Instant;

pub use languages::languages;

pub const CAPABILITY: muxy_core::capability::Capability =
    muxy_core::capability::Capability::Supported;
pub const LOCAL_MODEL: bool = true;

const SAMPLE_RATE: u32 = 16_000;
const BYTES_PER_SECOND: u32 = SAMPLE_RATE * 2;
const POLL: Duration = Duration::from_millis(50);
const MAX_AUDIO: usize = BYTES_PER_SECOND as usize * 600;
const MAX_TRANSCRIPT: usize = 256 * 1024;
const CAPTURE_IDLE: Duration = Duration::from_secs(10);
const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(600);
static ACTIVE: AtomicBool = AtomicBool::new(false);

struct Session;

impl Drop for Session {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

struct Tools {
    capture: PathBuf,
    transcribe: PathBuf,
    speech_model: PathBuf,
}

pub(super) fn start(language: String, model: String) -> Result<Recorder, String> {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return Err(tr!("Dictation is already active. Finish or cancel it first.").into());
    }
    let session = Session;
    let shared = Arc::new(Shared::default());
    let worker = shared.clone();
    std::thread::Builder::new()
        .name("offline-dictation".into())
        .spawn(move || {
            let _session = session;
            let result = prepare(&model, &worker).and_then(|(tools, model)| {
                let language = languages::resolve(&language)?;
                record(&tools, &model, &language, &worker)
            });
            if let Err(error) = result {
                let mut snapshot = worker
                    .snapshot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                snapshot.phase = Phase::Failed;
                snapshot.level = 0.0;
                snapshot.error = Some(error);
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(Recorder { shared })
}

fn prepare(model: &str, shared: &Shared) -> Result<(Tools, PathBuf), String> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            tr!("The user data folder is unavailable. Set XDG_DATA_HOME or HOME to use offline dictation.").to_string()
        })?
        .join("muxy/voice");
    let model = if model.is_empty() {
        data.join("ggml-base.bin")
    } else {
        PathBuf::from(model)
    };
    let model = validate_model(&model)?;
    let tools = Tools {
        capture: executable("parec")?,
        transcribe: executable("whisper-cli")?,
        speech_model: validate_speech_model(&data.join("ggml-silero-v6.2.0.bin"))?,
    };
    check_version(&tools.transcribe, shared)?;
    Ok((tools, model))
}

fn check_version(executable: &Path, shared: &Shared) -> Result<(), String> {
    let error = || tr!("Install whisper-cli 1.9.5 or later to use offline dictation.").to_string();
    let mut command = Command::new(executable);
    command.arg("--version");
    let mut process = Process::spawn(command, false).map_err(|_| error())?;
    let (success, output) = collect(&mut process, shared, Duration::from_secs(5), 1024)
        .map_err(|_| error())?
        .ok_or_else(error)?;
    let text = String::from_utf8(output).map_err(|_| error())?;
    let version = text
        .trim()
        .strip_prefix("whisper.cpp version: ")
        .and_then(|version| {
            let mut parts = version.split('.');
            Some((
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.split('-').next()?.parse::<u32>().ok()?,
            ))
        });
    if !success || version.is_none_or(|version| version < (1, 9, 5)) {
        return Err(error());
    }
    Ok(())
}

fn validate_model(path: &Path) -> Result<PathBuf, String> {
    let error = || {
        tr!("Choose a readable multilingual GGML voice model in Settings → Composer.").to_string()
    };
    let path = path.canonicalize().map_err(|_| error())?;
    if !path.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return Err(error());
    }
    let mut file = File::open(&path).map_err(|_| error())?;
    let mut header = [0; 8];
    file.read_exact(&mut header).map_err(|_| error())?;
    let vocabulary = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    if &header[..4] != b"lmgg" || vocabulary < 51_865 {
        return Err(error());
    }
    Ok(path)
}

fn validate_speech_model(path: &Path) -> Result<PathBuf, String> {
    let error = || {
        tr!(
            "Install the Silero speech detection model at %@ to use offline dictation.",
            path.display().to_string()
        )
        .to_string()
    };
    let canonical = path.canonicalize().map_err(|_| error())?;
    if !canonical
        .metadata()
        .is_ok_and(|metadata| metadata.is_file())
    {
        return Err(error());
    }
    let mut file = File::open(&canonical).map_err(|_| error())?;
    let mut header = [0; 18];
    file.read_exact(&mut header).map_err(|_| error())?;
    if &header != b"lmgg\x0a\0\0\0silero-16k" {
        return Err(error());
    }
    Ok(canonical)
}

fn executable(name: &str) -> Result<PathBuf, String> {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    let mut directories: Vec<_> = std::env::split_paths(&paths)
        .filter(|path| path.is_absolute())
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        directories.push(PathBuf::from(home).join(".local/bin"));
    }
    directories.extend([PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")]);
    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| tr!("Install %@ to use offline dictation.", name).into())
}

fn record(tools: &Tools, model: &Path, language: &str, shared: &Shared) -> Result<(), String> {
    let audio = capture(tools, shared, CAPTURE_IDLE, MAX_AUDIO)?;
    if shared.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    set_phase(shared, Phase::Transcribing);
    let transcript = if audio.is_empty() || audio.iter().all(|byte| *byte == 0) {
        String::new()
    } else {
        transcribe(tools, model, language, audio, shared, TRANSCRIPTION_TIMEOUT)?
    };
    if !shared.cancelled.load(Ordering::Acquire) {
        let mut snapshot = shared
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        snapshot.transcript = transcript;
        snapshot.phase = Phase::Finished;
    }
    Ok(())
}

fn local_server() -> Result<std::ffi::OsString, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::fs::metadata("/proc/self")
                .ok()
                .map(|metadata| PathBuf::from(format!("/run/user/{}", metadata.uid())))
        })
        .ok_or_else(|| {
            tr!("The local audio service is unavailable. Start dictation from a desktop session.")
                .to_string()
        })?;
    let mut server = std::ffi::OsString::from("unix:");
    server.push(runtime.join("pulse/native"));
    Ok(server)
}

fn capture(
    tools: &Tools,
    shared: &Shared,
    idle: Duration,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut audio = Vec::new();
    let mut capture = None;
    let mut last_audio = Instant::now();
    loop {
        if shared.cancelled.load(Ordering::Acquire) || shared.finishing.load(Ordering::Acquire) {
            audio.truncate(audio.len() / 2 * 2);
            return Ok(audio);
        }
        if audio.len() >= limit {
            shared.paused.store(true, Ordering::Release);
            shared
                .snapshot
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .error = Some(
                tr!("The 10-minute recording limit was reached. Finish to transcribe, or cancel.")
                    .into(),
            );
        }
        if shared.paused.load(Ordering::Acquire) {
            capture = None;
            audio.truncate(audio.len() / 2 * 2);
            set_phase(shared, Phase::Paused);
            std::thread::sleep(POLL);
            continue;
        }
        if capture.is_none() {
            let mut command = Command::new(&tools.capture);
            command.arg("--server").arg(local_server()?);
            command.args([
                "--raw",
                "--format=s16le",
                "--rate=16000",
                "--channels=1",
                "--device=@DEFAULT_SOURCE@",
                "--latency-msec=100",
                "--client-name=Atur Terminal",
                "--stream-name=Dictation",
            ]);
            capture = Some(Process::spawn(command, false)?);
            last_audio = Instant::now();
            set_phase(shared, Phase::Starting);
        }
        let Some(process) = capture.as_ref() else {
            continue;
        };
        match process.output.recv_timeout(POLL) {
            Ok(Ok(chunk)) => {
                last_audio = Instant::now();
                audio.extend_from_slice(&chunk[..chunk.len().min(limit - audio.len())]);
                let mut snapshot = shared.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
                snapshot.phase = Phase::Recording;
                snapshot.elapsed = Duration::from_millis(u64::try_from(audio.len()).unwrap_or(0) * 1000 / u64::from(BYTES_PER_SECOND));
                snapshot.level = level(&chunk);
            }
            Err(RecvTimeoutError::Timeout) if last_audio.elapsed() < idle => {}
            _ => return Err(tr!("Microphone capture stopped. Check the default input device and its PulseAudio or PipeWire permissions.").into()),
        }
    }
}

fn transcribe(
    tools: &Tools,
    model: &Path,
    language: &str,
    audio: Vec<u8>,
    shared: &Shared,
    timeout: Duration,
) -> Result<String, String> {
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    let mut command = Command::new(&tools.transcribe);
    command
        .arg("--model")
        .arg(model)
        .args(["--vad", "--vad-model"])
        .arg(&tools.speech_model)
        .args([
            "--language",
            language,
            "--file",
            "-",
            "--no-timestamps",
            "--no-prints",
            "--no-gpu",
            "--suppress-nst",
            "--output-txt",
            "--output-file",
            "-",
            "--threads",
        ])
        .arg(threads.to_string());
    let mut process = Process::spawn(command, true)?;
    let written = process.write(audio)?;
    let Some((success, output)) = collect(&mut process, shared, timeout, MAX_TRANSCRIPT)? else {
        return Ok(String::new());
    };
    if !success || !matches!(written.recv_timeout(POLL), Ok(Ok(()))) {
        return Err(tr!(
            "Offline transcription failed. Check whisper-cli, the voice model and the Silero speech detection model."
        )
        .into());
    }
    let text = String::from_utf8(output)
        .map_err(|_| tr!("Offline transcription returned invalid text.").to_string())?;
    Ok(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn collect(
    process: &mut Process,
    shared: &Shared,
    timeout: Duration,
    limit: usize,
) -> Result<Option<(bool, Vec<u8>)>, String> {
    let mut output = Vec::new();
    let mut ended = false;
    let deadline = Instant::now() + timeout;
    loop {
        if shared.cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(tr!(
                "Offline transcription timed out. Try a smaller model or a shorter recording."
            )
            .into());
        }
        if !ended {
            match process.output.recv_timeout(POLL) {
                Ok(Ok(chunk)) => {
                    if output.len() + chunk.len() > limit {
                        return Err(tr!("The offline transcript was too large.").into());
                    }
                    output.extend_from_slice(&chunk);
                }
                Ok(Err(error)) => return Err(error.to_string()),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => ended = true,
            }
            continue;
        }
        if let Some(status) = process.status()? {
            return Ok(Some((status.success(), output)));
        }
        std::thread::sleep(POLL);
    }
}

fn set_phase(shared: &Shared, phase: Phase) {
    let mut snapshot = shared
        .snapshot
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    snapshot.phase = phase;
    snapshot.level = 0.0;
    if phase == Phase::Transcribing {
        snapshot.error = None;
    }
}

fn level(bytes: &[u8]) -> f32 {
    let peak = bytes
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs())
        .max()
        .unwrap_or(0);
    (f32::from(peak) / 32768.0 * 4.0).min(1.0)
}

#[cfg(test)]
mod tests;
