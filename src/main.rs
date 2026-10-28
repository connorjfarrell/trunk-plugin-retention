//! retention — a Trunk Recorder Pro plugin that deletes the oldest calls from
//! the recordings folder once it is older, bigger, or a bigger share of its
//! disk than the settings allow.
//!
//! A pass runs a minute after recording starts and every ten minutes after.
//! Only a call's own files go ([`SUFFIXES`]), all of a call's together, the
//! oldest call first; nothing else in the folder is touched. A set of files is
//! a call when its name is the recorder's own (`<talkgroup>-<start>_<freq>`),
//! or one of them is a call JSON (a custom `filenameFormat` has no fixed name).
//! A call changed in the last [`RECENT`] may still be recording, or waiting for
//! a plugin, and stays. Folders left empty go, the recordings folder itself not.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sysinfo::Disks;
use trunk_recorder_plugin::{Host, Manifest, NoConfig, Plugin, Setup, State};

/// Between passes.
const EVERY: Duration = Duration::from_secs(600);
/// Before the first pass after start.
const FIRST: Duration = Duration::from_secs(60);
/// A call changed this recently is never removed.
const RECENT: Duration = Duration::from_secs(600);
/// What a call's files end in; the rest of the name is the call's.
const SUFFIXES: [&str; 5] = [".frames.jsonl", ".wav", ".m4a", ".json", ".sdr"];
/// What the size limit's gigabyte is.
const GB: f64 = 1e9;

/// The plugin's settings. Each limit is off at 0, and any that are set apply together.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "camelCase", default)]
struct Config {
    /// Keep calls for, days
    ///
    /// Older calls are deleted. 0: no limit.
    max_age_days: f64,
    /// Keep at most, GB
    ///
    /// The oldest calls go first (a gigabyte is 10⁹ bytes). 0: no limit.
    max_size_gb: f64,
    /// Keep at most, % of the disk
    ///
    /// Of the disk the recordings folder is on, from 1 to 95; the oldest calls go first. If a size is set too, the smaller limit wins. 0: no limit.
    max_disk_pct: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_age_days: 0.0, max_size_gb: 0.0, max_disk_pct: 0.0 }
    }
}

impl Config {
    fn check(&self) -> Result<(), String> {
        for (what, v) in [("Keep calls for", self.max_age_days), ("Keep at most, GB", self.max_size_gb), ("Keep at most, % of the disk", self.max_disk_pct)] {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("{what} must be 0 or more"));
            }
        }
        if self.max_disk_pct > 95.0 || (self.max_disk_pct > 0.0 && self.max_disk_pct < 1.0) {
            return Err("Keep at most, % of the disk is 0 (off) or from 1 to 95".into());
        }
        Ok(())
    }

    fn age_limit_days(&self) -> Option<f64> {
        (self.max_age_days > 0.0).then_some(self.max_age_days)
    }

    fn size_limit_gb(&self) -> Option<f64> {
        (self.max_size_gb > 0.0).then_some(self.max_size_gb)
    }

    fn disk_limit_pct(&self) -> Option<f64> {
        (self.max_disk_pct > 0.0).then_some(self.max_disk_pct)
    }

    fn active(&self) -> bool {
        self.age_limit_days().is_some() || self.size_limit_gb().is_some() || self.disk_limit_pct().is_some()
    }
}

/// One call's files.
#[derive(Debug)]
struct Call {
    files: Vec<(PathBuf, u64)>,
    /// The latest any of its files changed.
    newest: SystemTime,
    bytes: u64,
}

/// What a pass found and did.
#[derive(Debug, Default, PartialEq, Eq)]
struct Report {
    calls: usize,
    files: usize,
    bytes: u64,
    /// Why, as the log tells it.
    why: Vec<&'static str>,
    /// Files that couldn't be deleted, as the log tells it.
    failed: Vec<String>,
}

/// Whether `stem` is the recorder's own name for a call: `<talkgroup>-<start>_<freq>`,
/// the frequency with a slot or part number after it (`.0`, `.1`).
fn is_call_name(stem: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let Some((tg, rest)) = stem.split_once('-') else { return false };
    let Some((start, freq)) = rest.split_once('_') else { return false };
    digits(tg) && digits(start) && freq.split('.').all(digits)
}

/// Whether the file is a call's JSON: an object with a call's fields.
fn is_call_json(p: &Path) -> bool {
    use std::io::Read;
    let mut text = String::new();
    // (A call's JSON is a few KB; a larger file isn't one.)
    let Ok(f) = fs::File::open(p) else { return false };
    if f.take(64 * 1024).read_to_string(&mut text).is_err() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .is_ok_and(|v| ["talkgroup", "start_time", "call_length"].iter().all(|k| v.get(k).is_some_and(|x| x.is_number())))
}

/// The calls under `dir`, oldest first.
fn scan(dir: &Path) -> Vec<Call> {
    let mut by_call: HashMap<String, Call> = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            let p = e.path();
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                let Some(stem) = SUFFIXES.iter().find_map(|s| name.strip_suffix(s)) else { continue };
                let key = d.join(stem).display().to_string();
                // (A file with no time is taken as just written: it stays.)
                let t = m.modified().unwrap_or_else(|_| SystemTime::now());
                let c = by_call.entry(key).or_insert_with(|| Call { files: Vec::new(), newest: SystemTime::UNIX_EPOCH, bytes: 0 });
                c.files.push((p, m.len()));
                c.newest = c.newest.max(t);
                c.bytes += m.len();
            }
        }
    }
    // Files that merely end like a call's aren't one.
    let mut calls: Vec<Call> = by_call
        .into_iter()
        .filter(|(key, c)| {
            let stem = Path::new(key).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            is_call_name(&stem) || c.files.iter().any(|(p, _)| p.extension().is_some_and(|e| e == "json") && is_call_json(p))
        })
        .map(|(_, c)| c)
        .collect();
    calls.sort_by_key(|c| c.newest);
    calls
}

/// The size the folder may be, bytes: the smaller of the two limits set.
fn size_limit(r: &Config, disk_total: Option<u64>) -> Option<u64> {
    let gb = r.size_limit_gb().map(|g| (g * GB) as u64);
    let pct = r.disk_limit_pct().and_then(|p| disk_total.filter(|&t| t > 0).map(|t| (t as f64 * p / 100.0) as u64));
    gb.into_iter().chain(pct).min()
}

/// Which of `calls` (oldest first) to delete at `now`: the indexes, and why.
fn plan(calls: &[Call], r: &Config, disk_total: Option<u64>, now: SystemTime) -> (Vec<usize>, Vec<&'static str>) {
    let mut gone = vec![false; calls.len()];
    let mut why = Vec::new();
    // Oldest first, so what follows the first call too recent to remove is too.
    let removable = |c: &Call| now.duration_since(c.newest).is_ok_and(|d| d >= RECENT);
    if let Some(days) = r.age_limit_days() {
        // (A limit too big to be a Duration is no limit.)
        let age = Duration::try_from_secs_f64(days * 86_400.0).unwrap_or(Duration::MAX);
        for (i, c) in calls.iter().enumerate() {
            if removable(c) && now.duration_since(c.newest).is_ok_and(|d| d > age) {
                gone[i] = true;
            }
        }
        if gone.iter().any(|&g| g) {
            why.push("older than the age limit");
        }
    }
    if let Some(limit) = size_limit(r, disk_total) {
        let mut total: u64 = calls.iter().zip(&gone).filter(|(_, g)| !**g).map(|(c, _)| c.bytes).sum();
        let mut cut = false;
        for (i, c) in calls.iter().enumerate() {
            if total <= limit {
                break;
            }
            if gone[i] {
                continue;
            }
            if !removable(c) {
                break;
            }
            gone[i] = true;
            total -= c.bytes;
            cut = true;
        }
        if cut {
            why.push("over the size limit");
        }
    }
    ((0..calls.len()).filter(|&i| gone[i]).collect(), why)
}

/// One pass over `dir`.
fn pass(dir: &Path, r: &Config, disk_total: Option<u64>, now: SystemTime) -> Report {
    let calls = scan(dir);
    let (doomed, why) = plan(&calls, r, disk_total, now);
    let mut rep = Report { why, ..Default::default() };
    let mut parents = std::collections::BTreeSet::new();
    for i in doomed {
        let mut removed = 0;
        for (p, len) in &calls[i].files {
            match fs::remove_file(p) {
                Ok(()) => {
                    rep.files += 1;
                    rep.bytes += len;
                    removed += 1;
                    if let Some(d) = p.parent() {
                        parents.insert(d.to_path_buf());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => rep.failed.push(format!("{}: {e}", p.display())),
            }
        }
        rep.calls += usize::from(removed > 0);
    }
    // Folders that emptied, deepest first, up to (not including) `dir`.
    let mut dirs: Vec<PathBuf> = parents.into_iter().flat_map(|p| p.ancestors().take_while(|a| *a != dir).map(Path::to_path_buf).collect::<Vec<_>>()).collect();
    dirs.sort();
    dirs.dedup();
    for d in dirs.iter().rev() {
        let _ = fs::remove_dir(d);
    }
    rep
}

/// The capacity of the disk `path` is on.
fn disk_total(path: &Path) -> Option<u64> {
    let p = fs::canonicalize(path).ok()?;
    let disks = Disks::new_with_refreshed_list();
    let d = disks.list().iter().filter(|d| p.starts_with(d.mount_point())).max_by_key(|d| d.mount_point().as_os_str().len())?;
    Some(d.total_space())
}

struct Retention {
    /// Dropping it ends the worker.
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Plugin for Retention {
    type Config = Config;
    type SystemConfig = NoConfig;

    fn manifest() -> Manifest {
        Manifest {
            name: "Retention".into(),
            // It watches no events: it looks at the folder on a timer.
            subscribe: vec![],
            // The id, version, description, … come from Cargo.toml.
            ..trunk_recorder_plugin::manifest!()
        }
    }

    fn start(host: Host, setup: Setup<Config, NoConfig>) -> Result<Self, String> {
        let cfg = setup.config;
        cfg.check()?;
        let dir = setup.capture_dir;
        // The folder is deleted from: never the whole disk or a folder that isn't there.
        if dir.as_os_str().is_empty() || dir.parent().is_none() {
            return Err(format!("the recordings folder {} isn't a folder to delete calls from", dir.display()));
        }
        if !dir.is_dir() {
            return Err(format!("the recordings folder {} doesn't exist", dir.display()));
        }
        if cfg.active() {
            host.status(State::Ok, format!("keeping {} — first pass in a minute", describe(&cfg)));
        } else {
            host.status(State::Warning, "No limit is set, so nothing is deleted. Set one in this plugin's settings.");
        }
        let (stop, rx) = mpsc::channel::<()>();
        let worker = std::thread::Builder::new()
            .name("retention".into())
            .spawn(move || {
                let mut wait = FIRST;
                loop {
                    match rx.recv_timeout(wait) {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => return,
                    }
                    wait = EVERY;
                    if !cfg.active() {
                        continue;
                    }
                    // (A panic in a pass is said, not the end of the worker with the plugin looking well.)
                    let Ok(rep) = std::panic::catch_unwind(|| pass(&dir, &cfg, disk_total(&dir), SystemTime::now())) else {
                        host.status(State::Error, "A pass failed unexpectedly; it will try again in ten minutes. See the log.");
                        host.error("a pass panicked");
                        continue;
                    };
                    for f in &rep.failed {
                        host.warn(format!("couldn't delete {f}"));
                    }
                    if rep.files > 0 {
                        let line = format!(
                            "deleted {} call(s), {} file(s), {:.1} MB from {} ({})",
                            rep.calls,
                            rep.files,
                            rep.bytes as f64 / 1e6,
                            dir.display(),
                            rep.why.join(", ")
                        );
                        host.info(line.clone());
                        host.status(State::Ok, format!("keeping {}. Last pass: {line}", describe(&cfg)));
                    }
                }
            })
            .map_err(|e| format!("can't start the worker: {e}"))?;
        Ok(Retention { stop: Some(stop), worker: Some(worker) })
    }

    fn shutdown(&mut self, _grace: Duration) {
        self.stop = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// The limits as a sentence.
fn describe(c: &Config) -> String {
    let mut parts = Vec::new();
    if let Some(d) = c.age_limit_days() {
        parts.push(format!("{d} days"));
    }
    if let Some(g) = c.size_limit_gb() {
        parts.push(format!("{g} GB"));
    }
    if let Some(p) = c.disk_limit_pct() {
        parts.push(format!("{p}% of the disk"));
    }
    format!("at most {}", parts.join(" and "))
}

fn main() {
    trunk_recorder_plugin::run::<Retention>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use trunk_recorder_plugin::{HostMessage, testing};

    fn call(age_h: u64, bytes: u64, now: SystemTime) -> Call {
        Call { files: vec![(PathBuf::from(format!("/x/{age_h}-{bytes}.wav")), bytes)], newest: now - Duration::from_secs(age_h * 3600), bytes }
    }

    fn limits(days: f64, gb: f64, pct: f64) -> Config {
        Config { max_age_days: days, max_size_gb: gb, max_disk_pct: pct }
    }

    #[test]
    fn nothing_is_removed_with_no_limit() {
        let now = SystemTime::now();
        let calls = vec![call(5000, 10, now), call(1, 10, now)];
        assert_eq!(plan(&calls, &limits(0.0, 0.0, 0.0), None, now).0, Vec::<usize>::new());
    }

    #[test]
    fn age_removes_calls_older_than_the_limit() {
        let now = SystemTime::now();
        let calls = vec![call(80, 1, now), call(50, 1, now), call(20, 1, now)];
        let (gone, why) = plan(&calls, &limits(2.0, 0.0, 0.0), None, now);
        assert_eq!(gone, [0, 1]);
        assert_eq!(why, ["older than the age limit"]);
    }

    #[test]
    fn size_removes_the_oldest_until_it_fits() {
        let now = SystemTime::now();
        let gb = GB as u64;
        let calls = vec![call(30, gb, now), call(20, gb, now), call(10, gb, now), call(5, gb, now)];
        let (gone, why) = plan(&calls, &limits(0.0, 2.5, 0.0), None, now);
        assert_eq!(gone, [0, 1], "4 GB down to 2");
        assert_eq!(why, ["over the size limit"]);
        assert!(plan(&calls, &limits(0.0, 4.0, 0.0), None, now).0.is_empty(), "exactly at the limit is fine");
    }

    #[test]
    fn the_share_of_the_disk_and_the_size_limit_apply_together() {
        let now = SystemTime::now();
        let gb = GB as u64;
        let calls = vec![call(30, gb, now), call(20, gb, now), call(10, gb, now), call(5, gb, now)];
        // 10% of a 20 GB disk is 2 GB; the size limit says 3.
        assert_eq!(plan(&calls, &limits(0.0, 3.0, 10.0), Some(20 * gb), now).0, [0, 1]);
        // 50% of it is 10 GB; the size limit says 3.
        assert_eq!(plan(&calls, &limits(0.0, 3.0, 50.0), Some(20 * gb), now).0, [0]);
        // A share of a disk of unknown size does nothing.
        assert!(plan(&calls, &limits(0.0, 0.0, 10.0), None, now).0.is_empty());
    }

    #[test]
    fn age_and_size_together_count_only_what_is_left() {
        let now = SystemTime::now();
        let gb = GB as u64;
        // Age removes the first; the remaining 2 GB fit in 2.5.
        let calls = vec![call(80, gb, now), call(10, gb, now), call(5, gb, now)];
        assert_eq!(plan(&calls, &limits(2.0, 2.5, 0.0), None, now).0, [0]);
    }

    #[test]
    fn a_recent_call_is_never_removed() {
        let now = SystemTime::now();
        let gb = GB as u64;
        let mut calls = vec![call(5, gb, now), call(0, gb, now)];
        calls[1].newest = now - Duration::from_secs(30);
        let (gone, _) = plan(&calls, &limits(0.0, 0.5, 0.0), None, now);
        assert_eq!(gone, [0], "the one 30 s old stays, though the folder is still over");
        let (gone, _) = plan(&calls, &limits(0.000001, 0.0, 0.0), None, now);
        assert_eq!(gone, [0]);
    }

    fn write(p: &Path, n: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; n]).unwrap();
    }

    fn set_age(p: &Path, now: SystemTime, hours: u64) {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(now - Duration::from_secs(hours * 3600)).unwrap();
    }

    #[test]
    fn a_pass_deletes_whole_calls_and_only_calls() {
        let root = std::env::temp_dir().join(format!("retention-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let now = SystemTime::now();
        let old = root.join("a/2026/01/01");
        let new = root.join("a/2026/10/13");
        for ext in ["wav", "json", "m4a", "frames.jsonl", "sdr"] {
            write(&old.join(format!("5-100_851.0.{ext}")), 10);
            set_age(&old.join(format!("5-100_851.0.{ext}")), now, 24 * 60);
        }
        write(&old.join("notes.txt"), 10);
        set_age(&old.join("notes.txt"), now, 24 * 60);
        write(&new.join("5-200_851.0.wav"), 10);
        write(&new.join("5-200_851.0.json"), 10);
        set_age(&new.join("5-200_851.0.wav"), now, 2);
        set_age(&new.join("5-200_851.0.json"), now, 2);
        let rep = pass(&root, &limits(30.0, 0.0, 0.0), None, now);
        assert_eq!((rep.calls, rep.files, rep.bytes), (1, 5, 50));
        assert!(old.join("notes.txt").exists(), "other files stay, and so does their folder");
        assert!(!old.join("5-100_851.0.wav").exists() && !old.join("5-100_851.0.json").exists());
        assert!(new.join("5-200_851.0.wav").exists() && new.join("5-200_851.0.json").exists());
        // Delete the note: the folders that held only the old call go on the next pass.
        fs::remove_file(old.join("notes.txt")).unwrap();
        write(&old.join("5-100_851.0.wav"), 10);
        set_age(&old.join("5-100_851.0.wav"), now, 24 * 60);
        pass(&root, &limits(30.0, 0.0, 0.0), None, now);
        assert!(!root.join("a/2026/01").exists(), "emptied folders go");
        assert!(root.exists() && new.exists(), "the recordings folder and busy ones stay");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn only_a_calls_own_files_are_calls() {
        assert!(is_call_name("5-1791952226_851012500") && is_call_name("1125-1791952226_852650000.1") && is_call_name("5-1_2.0.1"));
        for no in ["notes", "audio", "5-1791952226", "-1_2", "a-1_2", "5-1_", "5-1_2.x", "5-1_2."] {
            assert!(!is_call_name(no), "{no}");
        }
        let root = std::env::temp_dir().join(format!("retention-id-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let now = SystemTime::now();
        let call_json = br#"{"talkgroup":5,"start_time":1,"call_length":3,"freq":851000000}"#;
        // A custom-named call, known by its JSON; stray files with call-like endings.
        fs::create_dir_all(&root).unwrap();
        for (n, body) in
            [("kcpd-dispatch.json", &call_json[..]), ("kcpd-dispatch.wav", b"x"), ("notes.json", br#"{"a":1}"#), ("audio.wav", b"x"), ("list.json", b"[1,2]")]
        {
            fs::write(root.join(n), body).unwrap();
            set_age(&root.join(n), now, 24 * 90);
        }
        let rep = pass(&root, &limits(30.0, 0.0, 0.0), None, now);
        assert_eq!((rep.calls, rep.files), (1, 2));
        assert!(!root.join("kcpd-dispatch.wav").exists() && !root.join("kcpd-dispatch.json").exists());
        for keep in ["notes.json", "audio.wav", "list.json"] {
            assert!(root.join(keep).exists(), "{keep}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let root = std::env::temp_dir().join(format!("retention-links-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("retention-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        let now = SystemTime::now();
        write(&outside.join("5-100_851.0.wav"), 10);
        set_age(&outside.join("5-100_851.0.wav"), now, 24 * 90);
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(outside.join("5-100_851.0.wav"), root.join("5-200_851.0.wav")).unwrap();
        let rep = pass(&root, &limits(30.0, 0.0, 0.0), None, now);
        assert_eq!((rep.calls, rep.files), (0, 0));
        assert!(outside.join("5-100_851.0.wav").exists(), "a call outside the folder, reached through a link, stays");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn an_age_too_big_to_be_a_duration_is_no_limit() {
        let now = SystemTime::now();
        let calls = vec![call(5000, 1, now)];
        assert!(plan(&calls, &limits(1e308, 0.0, 0.0), None, now).0.is_empty());
    }

    #[test]
    fn settings_are_checked() {
        assert!(limits(0.0, 0.0, 0.0).check().is_ok());
        assert!(limits(90.0, 0.0, 80.0).check().is_ok());
        assert!(limits(-1.0, 0.0, 0.0).check().is_err());
        assert!(limits(0.0, f64::NAN, 0.0).check().is_err());
        assert!(limits(0.0, 0.0, 96.0).check().is_err());
        assert!(limits(0.0, 0.0, 0.5).check().is_err());
    }

    #[test]
    fn it_starts_and_stops_with_good_settings() {
        let dir = testing::temp_dir("retention");
        let hello = testing::hello(&dir, json!({ "maxAgeDays": 90 }));
        let out = testing::run::<Retention>([HostMessage::Hello(hello)]);
        assert!(out.ready());
    }

    #[test]
    fn bad_settings_stop_it() {
        let dir = testing::temp_dir("retention");
        let hello = testing::hello(&dir, json!({ "maxDiskPct": 99 }));
        let out = testing::run::<Retention>([HostMessage::Hello(hello)]);
        assert!(!out.ready());
        assert_eq!(out.exit_code, trunk_recorder_plugin::EXIT_CONFIG);
    }
}
