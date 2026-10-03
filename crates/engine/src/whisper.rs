//! Offline speech-to-text via the bundled whisper.cpp sidecar (Phase 22,
//! TEXT-02 / EYES-03).
//!
//! This module is the ONE genuinely-new engineering surface Phase 22 adds:
//! locate the bundled `whisper-cli.exe` + `ggml-small.bin` (bundled-first,
//! mirroring [`crate::ffmpeg::locate`]), extract a media window to the 16 kHz
//! mono WAV whisper.cpp requires (via
//! [`crate::ffmpeg::extract_wav_for_whisper`]), run whisper with the
//! Plan-01-RESOLVED flags, and parse its `-ojf` JSON into a typed word list
//! with source-relative microsecond timestamps.
//!
//! ## Resolved flags (source of truth: `scripts/windows/fetch-whisper-cli.ps1`)
//! Recorded VERBATIM from the real whisper.cpp v1.9.1 `--help` in Plan 01 — NOT
//! guessed:
//!   * `-dtw small`  — token/word DTW timestamps (preset matches `ggml-small.bin`;
//!     NOT `-ml 1`, which is a cruder heuristic — Pitfall 2).
//!   * `-ojf`        — `--output-json-full`: per-token timing detail.
//!   * `-of <base>`  — output file path WITHOUT extension (`<base>.json` written).
//!   * `-f <wav>`    — input 16 kHz mono WAV.
//!   * `-m <model>`  — the GGML model path.
//!   * `-l auto`     — auto-detect language (proven on both the English AND the
//!     Spanish fixture — one code path, any well-supported language, SC-1).
//!
//! ## Offline
//! Both the binary and the model are bundled LOCAL files (PROVENANCE Entry 11).
//! Transcription touches NO network — the `#[ignore]` integration test running
//! green with the machine offline is the proof (CLAUDE.md offline-core rule).
//!
//! Zero new Cargo deps: `serde`/`serde_json` (already present) parse the JSON;
//! `std::process::Command` (already used for ffmpeg) spawns the sidecar.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::ffmpeg::{extract_wav_for_whisper, unique_temp_stem};
use crate::EngineError;

/// Hard ceiling on a single transcription window: 30 minutes. A pathologically
/// long window is REJECTED before any WAV extraction (resource-exhaustion cap,
/// threat T-22-06) — mirroring the compositor's hard count ceilings.
pub const WHISPER_MAX_WINDOW_US: i64 = 30 * 60 * 1_000_000;

/// Reject a transcription window longer than [`WHISPER_MAX_WINDOW_US`]
/// (resource-exhaustion cap, threat T-22-06).
///
/// SINGLE source of both the cap value and its error message. Both the
/// public entry point ([`transcribe_media_window`]) and the natural choke
/// point ([`crate::ffmpeg::extract_wav_for_whisper`], where every WAV
/// extraction — including alternate callers — must pass) call this, so the
/// wording can never drift between the two sites (defense-in-depth without
/// duplication).
pub(crate) fn check_whisper_window_cap(in_us: i64, out_us: i64) -> Result<(), EngineError> {
    if out_us - in_us > WHISPER_MAX_WINDOW_US {
        return Err(EngineError::SidecarFailed {
            tool: "whisper-cli".to_string(),
            status: -1,
            stderr: format!(
                "transcription window {}us exceeds the {}us ({}min) ceiling",
                out_us - in_us,
                WHISPER_MAX_WINDOW_US,
                WHISPER_MAX_WINDOW_US / 60_000_000
            ),
        });
    }
    Ok(())
}

/// One transcribed word with per-word timestamps.
///
/// `start_us`/`end_us` are microseconds. As returned by [`parse_whisper_json`]
/// / [`transcribe_wav`] they are RELATIVE to the extracted window's start; the
/// media-window helper [`transcribe_media_window`] adds the window's `in_us`
/// offset so its returned words carry ABSOLUTE source-media time (the value
/// downstream tools — captions, word-cuts, search — map from).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_us: i64,
    pub end_us: i64,
}

// ---------------------------------------------------------------------------
// whisper.cpp -ojf JSON wire schema (only the fields we consume)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WhisperOutput {
    #[serde(default)]
    transcription: Vec<WhisperSegment>,
}

#[derive(Debug, Deserialize)]
struct WhisperSegment {
    #[serde(default)]
    tokens: Vec<WhisperToken>,
}

#[derive(Debug, Deserialize)]
struct WhisperToken {
    text: String,
    offsets: WhisperOffsets,
}

/// Per-token offsets in MILLISECONDS (whisper.cpp emits `offsets.{from,to}` as
/// integer ms — verified against the real v1.9.1 `-ojf` output).
#[derive(Debug, Deserialize)]
struct WhisperOffsets {
    from: i64,
    to: i64,
}

/// True for whisper's non-content special tokens — the begin marker `[_BEG_]`,
/// end `[_EOT_]`, and the timestamp tokens `[_TT_nnn]`. All are wrapped in
/// `[_..._]`, so a single prefix check filters every variant.
fn is_special_token(text: &str) -> bool {
    text.starts_with("[_")
}

/// Parse a whisper.cpp `-ojf` JSON document into a `Vec<Word>` with
/// source-relative microsecond timestamps.
///
/// whisper tokenizes into sub-word pieces where a leading SPACE marks a new
/// word (GPT-2 BPE convention); punctuation attaches to the preceding word with
/// no leading space. We group tokens into words on that boundary, filtering the
/// special `[_..._]` tokens, converting `offsets` (ms) to µs, and taking each
/// word's start from its first token and end from its last. Empty/whitespace-
/// only words are dropped.
///
/// A malformed or empty document is a clean `Err(ProbeParse)` — never a panic
/// (the whole engine's crash-isolation contract).
pub fn parse_whisper_json(json: &str) -> Result<Vec<Word>, EngineError> {
    let parsed: WhisperOutput = serde_json::from_str(json)
        .map_err(|e| EngineError::ProbeParse(format!("invalid whisper JSON: {e}")))?;

    let mut words: Vec<Word> = Vec::new();
    for seg in &parsed.transcription {
        for tok in &seg.tokens {
            if is_special_token(&tok.text) {
                continue;
            }
            let start_us = tok.offsets.from.saturating_mul(1000);
            let end_us = tok.offsets.to.saturating_mul(1000);
            let starts_word = tok.text.starts_with(' ');
            let piece = tok.text.trim();

            if starts_word || words.is_empty() {
                if piece.is_empty() {
                    // A lone leading-space token with no glyphs (rare): skip it
                    // rather than push an empty word.
                    continue;
                }
                words.push(Word {
                    text: piece.to_string(),
                    start_us,
                    end_us,
                });
            } else {
                // Continuation / trailing punctuation — append to the current
                // word and extend its end.
                let w = words.last_mut().expect("words non-empty in else branch");
                w.text.push_str(piece);
                if end_us > w.end_us {
                    w.end_us = end_us;
                }
            }
        }
    }

    // A word whose grouped tokens collapsed to zero width (e.g. a leading-space
    // token immediately followed by end-of-stream) would violate end > start;
    // give it a 1µs minimum so downstream ordering math stays well-defined.
    for w in &mut words {
        if w.end_us <= w.start_us {
            w.end_us = w.start_us + 1;
        }
    }

    Ok(words)
}

// ---------------------------------------------------------------------------
// Keyword search over a transcript (EYES-03) — pure, offline, I/O-free
// ---------------------------------------------------------------------------

/// One contiguous run of transcript words matching a search query, expressed as
/// a directly-PLACEABLE source-media time span (EYES-03). `start_us` is the
/// first matched word's start, `end_us` the last matched word's end — both
/// ABSOLUTE source-media microseconds (whatever coordinate space the input
/// `Word`s carry), so the span can be placed as a clip source range unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WordMatch {
    /// The matched words joined by single spaces (their original text).
    pub text: String,
    /// Source-media start of the run (inclusive), microseconds.
    pub start_us: i64,
    /// Source-media end of the run (exclusive), microseconds.
    pub end_us: i64,
}

/// Normalize one word/token for matching: keep only alphanumerics (dropping
/// surrounding punctuation like the `.` in `"dog."` or an apostrophe), folded to
/// lowercase. Unicode-aware, so accented Spanish letters survive (SC-1's ES
/// path). Returns the empty string for a pure-punctuation token.
fn normalize_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Find every contiguous run of `words` whose normalized text equals the
/// (case-insensitive, whitespace-normalized, punctuation-stripped) `query`
/// phrase, returning each as a placeable source-time [`WordMatch`] span
/// (EYES-03). Pure string/slice work — NO I/O, unit-testable offline.
///
/// Matching is phrase-exact on normalized tokens: query `"brown fox"` matches
/// the adjacent word pair `"brown"`,`"fox"` (case-insensitively, ignoring
/// trailing punctuation) and yields the span from the first word's start to the
/// last word's end. Matches never overlap — after a hit the scan resumes past
/// the matched run. An empty/whitespace-only query returns no matches.
pub fn search_words(words: &[Word], query: &str) -> Vec<WordMatch> {
    let q: Vec<String> = query
        .split_whitespace()
        .map(|t| normalize_token(t))
        .filter(|t| !t.is_empty())
        .collect();
    if q.is_empty() {
        return Vec::new();
    }

    let norm: Vec<String> = words.iter().map(|w| normalize_token(&w.text)).collect();
    let k = q.len();
    let mut matches = Vec::new();
    let mut i = 0;
    while i + k <= words.len() {
        if norm[i..i + k] == q[..] {
            let text = words[i..i + k]
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            matches.push(WordMatch {
                text,
                start_us: words[i].start_us,
                end_us: words[i + k - 1].end_us,
            });
            i += k; // non-overlapping: resume past this run
        } else {
            i += 1;
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// Bundled-sidecar location (mirrors ffmpeg::resolve_binary's bundled-first order)
// ---------------------------------------------------------------------------

/// Build a `Command` for the whisper-cli binary with the Windows console window
/// suppressed (mirrors `ffmpeg::ffmpeg_command`). Every whisper spawn goes
/// through here so a transcription never flashes a console window during
/// playback/agent use.
fn whisper_command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The bundled-first candidate directories to search for the whisper sidecar +
/// model, in the SAME order as [`crate::ffmpeg`]'s `resolve_binary`:
///   1. `RUDIS_WHISPER_DIR` — the explicit bundled dir the packaged app exports
///      at startup (`native_surface::point_engine_at_bundled_whisper`).
///   2. The current exe's dir and its `binaries/` subdir (packaged layout).
fn whisper_candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("RUDIS_WHISPER_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            dirs.push(exe_dir.to_path_buf());
            dirs.push(exe_dir.join("binaries"));
        }
    }
    dirs
}

/// The whisper CLI executable names to search for in the CONTROLLED bundled
/// candidate dirs (dirs the app itself resolved). Prefers the current
/// `whisper-cli` name, falling back to the legacy `main` name (Assumption A2).
/// The generic `main` name is ONLY honored here — never in the open PATH scan
/// (see [`whisper_cli_path_name`], WR-02).
fn whisper_cli_names() -> [&'static str; 2] {
    #[cfg(windows)]
    {
        ["whisper-cli.exe", "main.exe"]
    }
    #[cfg(not(windows))]
    {
        ["whisper-cli", "main"]
    }
}

/// The DISTINCTIVE whisper CLI name used for the open `PATH` dev-fallback scan.
/// Deliberately excludes the generic `main`/`main.exe` (an extremely common
/// executable name) so the PATH scan can never spawn an unrelated binary with
/// whisper's argv — mirroring `ffmpeg`/`ffprobe`'s narrow PATH search (WR-02).
fn whisper_cli_path_name() -> &'static str {
    #[cfg(windows)]
    {
        "whisper-cli.exe"
    }
    #[cfg(not(windows))]
    {
        "whisper-cli"
    }
}

/// Locate the bundled `whisper-cli` executable and its `ggml-small.bin` model,
/// bundled-first (see [`whisper_candidate_dirs`]) — mirroring
/// [`crate::ffmpeg::locate`]. Returns `(whisper_cli, model)`.
///
/// Errors with [`EngineError::BinaryNotFound`] when EITHER the executable or the
/// model cannot be found (whisper is useless without both), so the caller fails
/// cleanly instead of spawning a doomed process. The PATH fallback covers a
/// whisper-cli on `PATH` for dev, but the model must still sit in one of the
/// bundled candidate dirs (models are never on `PATH`).
pub fn locate_whisper() -> Result<(PathBuf, PathBuf), EngineError> {
    let dirs = whisper_candidate_dirs();

    // Executable: bundled candidate dirs first, then PATH (dev fallback).
    let cli = dirs
        .iter()
        .flat_map(|d| whisper_cli_names().map(|n| d.join(n)))
        .find(|p| p.is_file())
        .or_else(|| find_whisper_on_path());

    // Model: bundled candidate dirs only (never on PATH).
    let model = dirs
        .iter()
        .map(|d| d.join("ggml-small.bin"))
        .find(|p| p.is_file());

    match (cli, model) {
        (Some(cli), Some(model)) => Ok((cli, model)),
        (None, _) => Err(EngineError::BinaryNotFound("whisper-cli".to_string())),
        (_, None) => Err(EngineError::BinaryNotFound("ggml-small.bin".to_string())),
    }
}

/// Search `PATH` for the distinctive `whisper-cli` executable (dev fallback).
/// `None` if absent. Only the distinctive name is searched (NOT the generic
/// `main`) so an unrelated binary earlier on `PATH` can never be spawned with
/// whisper's argv (WR-02).
fn find_whisper_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let name = whisper_cli_path_name();
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(name);
        // Verify the exec bit (Unix), not just existence — mirroring
        // `ffmpeg::find_on_path` so a non-executable file named `whisper-cli`
        // on PATH is SKIPPED during the search rather than failing at spawn
        // (IN-04). On Windows `is_executable` reduces to the `.exe` presence
        // check.
        if crate::ffmpeg::is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Transcription
// ---------------------------------------------------------------------------

/// Run `whisper_cli` on a 16 kHz mono WAV and parse its `-ojf` JSON into words
/// with WINDOW-RELATIVE microsecond timestamps.
///
/// Invokes the sidecar with the Plan-01-RESOLVED argv — `-m <model> -dtw small
/// -ojf -of <base> -l auto -f <wav>` — reads the `<base>.json` whisper writes,
/// parses it via [`parse_whisper_json`], and deletes the temp JSON. Every path
/// is a separate `.arg()` (no shell interpolation — threat T-22-04). A non-zero
/// exit is [`EngineError::SidecarFailed`]; a missing/garbled JSON is a clean
/// `Err`, never a panic.
pub fn transcribe_wav(
    wav_path: &Path,
    whisper_cli: &Path,
    model: &Path,
) -> Result<Vec<Word>, EngineError> {
    let base = std::env::temp_dir().join(format!("rudis-whisper-out-{}", unique_temp_stem()));
    let json_path = base.with_extension("json");

    let output = whisper_command(whisper_cli)
        .arg("-m")
        .arg(model)
        .args(["-dtw", "small"])
        .arg("-ojf")
        .arg("-of")
        .arg(&base)
        .args(["-l", "auto"])
        .arg("-f")
        .arg(wav_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&json_path);
        return Err(EngineError::SidecarFailed {
            tool: "whisper-cli".to_string(),
            status: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    // Read the bytes whisper wrote, then delete the temp JSON UNCONDITIONALLY
    // before parsing — `String::from_utf8_lossy` never fails, so no early-return
    // path (invalid UTF-8, parse error) can leak the transcript JSON onto disk
    // (threat T-22-07). A missing/unreadable JSON is the sidecar's "no output"
    // failure, mapped to a clean `Err`.
    let bytes = std::fs::read(&json_path).map_err(|e| EngineError::SidecarFailed {
        tool: "whisper-cli".to_string(),
        status: 0,
        stderr: format!("whisper wrote no JSON output at {}: {e}", json_path.display()),
    })?;
    let _ = std::fs::remove_file(&json_path);
    let json = String::from_utf8_lossy(&bytes);

    parse_whisper_json(&json)
}

/// RAII guard that deletes a temp WAV on EVERY exit path (normal return, `?`
/// early-return, or panic unwind) so extracted speech content never lingers on
/// disk (threat T-22-07, Information Disclosure).
struct TempWavGuard(PathBuf);

impl Drop for TempWavGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Transcribe the audio of `path` over the media window `[in_us, out_us)` into
/// words with ABSOLUTE source-media timestamps.
///
/// Steps:
///   1. Reject a window longer than [`WHISPER_MAX_WINDOW_US`] BEFORE any
///      extraction (resource cap, threat T-22-06).
///   2. [`extract_wav_for_whisper`] → `Ok(None)` (no audio stream) returns an
///      EMPTY word list (silence is data — research Pitfall 4).
///   3. [`locate_whisper`] → [`transcribe_wav`] on the extracted 16 kHz mono
///      WAV. The temp WAV is wrapped in a [`TempWavGuard`] so it is deleted even
///      if transcription errors or panics.
///   4. Add `in_us.max(0)` to each word's start/end so returned times are
///      ABSOLUTE source-media time (the window-relative times whisper emits +
///      the window's own offset). The `.max(0)` clamp mirrors
///      [`extract_wav_for_whisper`]'s own clamped seek start, so the offset
///      applied here matches the window actually extracted (differs from a raw
///      `in_us` only for a negative `in_us`, which real clip windows never
///      produce).
pub fn transcribe_media_window(
    path: &Path,
    in_us: i64,
    out_us: i64,
) -> Result<Vec<Word>, EngineError> {
    if out_us <= in_us {
        return Err(EngineError::InvalidWindow { in_us, out_us });
    }
    check_whisper_window_cap(in_us, out_us)?;

    let wav = match extract_wav_for_whisper(path, in_us, out_us)? {
        Some(w) => w,
        None => return Ok(Vec::new()), // no audio stream: silence is data
    };
    let _guard = TempWavGuard(wav.clone()); // delete the temp WAV on any exit

    let (cli, model) = locate_whisper()?;
    let mut words = transcribe_wav(&wav, &cli, &model)?;

    // Window-relative -> absolute source-media time.
    let offset = in_us.max(0);
    for w in &mut words {
        w.start_us += offset;
        w.end_us += offset;
    }
    Ok(words)
}
