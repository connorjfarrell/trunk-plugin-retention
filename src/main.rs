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

use std::collections::{BTreeMap, HashMap};
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
    /// Dry run
    ///
    /// Say in the log what a pass would delete, and delete nothing. Turn it on when you set or change a limit, look at what it says, then turn it off.
    dry_run: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_age_days: 0.0, max_size_gb: 0.0, max_disk_pct: 0.0, dry_run: false }
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

/// What a pass found and did (or, in a dry run, would do).
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

/// The limits of a pass, in the terms it works in.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Limits {
    age: Option<Duration>,
    /// The size the folder may be, bytes: the smaller of the size and the share of the disk.
    bytes: Option<u64>,
}

impl Limits {
    fn new(r: &Config, disk_total: Option<u64>) -> Limits {
        // (A limit too big to be a Duration is no limit.)
        let age = r.age_limit_days().and_then(|d| Duration::try_from_secs_f64(d * 86_400.0).ok());
        let gb = r.size_limit_gb().map(|g| (g * GB) as u64);
        let pct = r.disk_limit_pct().and_then(|p| disk_total.filter(|&t| t > 0).map(|t| (t as f64 * p / 100.0) as u64));
        Limits { age, bytes: gb.into_iter().chain(pct).min() }
    }
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

/// Hands each call under `dir` to `f`, a folder at a time: only one folder's files are held, so a
/// recordings folder of millions of calls costs no more memory than its busiest day.
fn walk(dir: &Path, mut f: impl FnMut(Call)) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut by_call: HashMap<String, Call> = HashMap::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            // (`DirEntry::metadata` doesn't follow a link: a link is neither a folder nor a file here.)
            let Ok(m) = e.metadata() else { continue };
            let p = e.path();
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file() {
                let Some(stem) = SUFFIXES.iter().find_map(|s| name.strip_suffix(s)) else { continue };
                // (A file with no time is taken as just written: it stays.)
                let t = m.modified().unwrap_or_else(|_| SystemTime::now());
                let c = by_call.entry(stem.to_string()).or_insert_with(|| Call { files: Vec::new(), newest: SystemTime::UNIX_EPOCH, bytes: 0 });
                c.files.push((p, m.len()));
                c.newest = c.newest.max(t);
                c.bytes += m.len();
            }
        }
        for (stem, c) in by_call {
            // Files that merely end like a call's aren't one.
            if is_call_name(&stem) || c.files.iter().any(|(p, _)| p.extension().is_some_and(|e| e == "json") && is_call_json(p)) {
                f(c);
            }
        }
    }
}

fn secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The time before which calls go, as Unix seconds, and the sizes behind it: calls whose newest file
/// is older than this are deleted. `sizes` is the bytes of calls by the minute they last changed.
/// (A size limit is met to the minute: up to a minute more than needed may go.)
fn cutoff(sizes: &BTreeMap<i64, u64>, limits: &Limits, now: SystemTime) -> (i64, i64) {
    let now_s = secs(now);
    // Never a call changed in the last [`RECENT`].
    let newest_allowed = now_s - RECENT.as_secs() as i64;
    let age_cut = limits.age.map_or(i64::MIN, |a| now_s.saturating_sub(a.as_secs().min(i64::MAX as u64) as i64));
    let mut size_cut = i64::MIN;
    if let Some(limit) = limits.bytes {
        // What the folder holds once the old ones are gone.
        let mut total: u64 = sizes.iter().filter(|(m, _)| (**m + 1) * 60 > age_cut).map(|(_, b)| b).sum();
        for (m, bytes) in sizes {
            if total <= limit {
                break;
            }
            if (m + 1) * 60 <= age_cut {
                continue;
            }
            total -= bytes;
            size_cut = (m + 1) * 60;
        }
    }
    (age_cut.min(newest_allowed), size_cut.min(newest_allowed))
}

/// One pass over `dir`: a walk to total the calls by the minute they last changed, which says where
/// the line is, and a second walk to delete the calls before it. With `dry_run`, nothing is deleted
/// and the report is what would be.
fn pass(dir: &Path, limits: &Limits, now: SystemTime, dry_run: bool) -> Report {
    let mut rep = Report::default();
    if limits.age.is_none() && limits.bytes.is_none() {
        return rep;
    }
    let mut sizes: BTreeMap<i64, u64> = BTreeMap::new();
    walk(dir, |c| *sizes.entry(secs(c.newest).div_euclid(60)).or_default() += c.bytes);
    let (age_cut, size_cut) = cutoff(&sizes, limits, now);
    let (mut by_age, mut by_size) = (false, false);
    let mut parents = std::collections::BTreeSet::new();
    walk(dir, |c| {
        let t = secs(c.newest);
        if t >= age_cut && t >= size_cut {
            return;
        }
        let mut removed = 0;
        for (p, len) in &c.files {
            if dry_run {
                rep.files += 1;
                rep.bytes += len;
                removed += 1;
                continue;
            }
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
        if removed > 0 {
            rep.calls += 1;
            if t < age_cut {
                by_age = true;
            } else {
                by_size = true;
            }
        }
    });
    if by_age {
        rep.why.push("older than the age limit");
    }
    if by_size {
        rep.why.push("over the size limit");
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
            let how = if cfg.dry_run { "dry run (nothing is deleted)" } else { "keeping" };
            host.status(State::Ok, format!("{how} {} — first pass in a minute", describe(&cfg)));
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
                    let Ok(rep) = std::panic::catch_unwind(|| pass(&dir, &Limits::new(&cfg, disk_total(&dir)), SystemTime::now(), cfg.dry_run)) else {
                        host.status(State::Error, "A pass failed unexpectedly; it will try again in ten minutes. See the log.");
                        host.error("a pass panicked");
                        continue;
                    };
                    for f in &rep.failed {
                        host.warn(format!("couldn't delete {f}"));
                    }
                    if rep.files > 0 {
                        let line = format!(
                            "{} {} call(s), {} file(s), {:.1} MB from {} ({})",
                            if cfg.dry_run { "dry run: would delete" } else { "deleted" },
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

    fn limits(days: f64, bytes: u64) -> Limits {
        Limits { age: (days > 0.0).then(|| Duration::from_secs_f64(days * 86_400.0)), bytes: (bytes > 0).then_some(bytes) }
    }

    fn write(p: &Path, n: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; n]).unwrap();
    }

    fn set_age(p: &Path, now: SystemTime, hours: u64) {
        let f = fs::File::options().write(true).open(p).unwrap();
        f.set_modified(now - Duration::from_secs(hours * 3600)).unwrap();
    }

    fn root(name: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("retention-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&r);
        fs::create_dir_all(&r).unwrap();
        r
    }

    /// A call of `bytes`, last changed `age_h` hours ago, in `d` (a `.wav` and a `.json`).
    fn call(d: &Path, tg: u32, age_h: u64, bytes: usize, now: SystemTime) {
        for (ext, n) in [("wav", bytes - 10), ("json", 10)] {
            let p = d.join(format!("{tg}-{}_851.0.{ext}", age_h + 1_000_000));
            write(&p, n);
            set_age(&p, now, age_h);
        }
    }

    fn left(d: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".wav")).collect();
        v.sort();
        v
    }

    #[test]
    fn nothing_is_removed_with_no_limit() {
        let (r, now) = (root("none"), SystemTime::now());
        call(&r, 5, 5000, 100, now);
        let rep = pass(&r, &limits(0.0, 0), now, false);
        assert_eq!((rep.calls, rep.files), (0, 0));
        assert_eq!(left(&r).len(), 1);
    }

    #[test]
    fn age_removes_calls_older_than_the_limit() {
        let (r, now) = (root("age"), SystemTime::now());
        for (tg, age_h) in [(1, 80 * 24), (2, 50 * 24), (3, 20 * 24)] {
            call(&r, tg, age_h, 100, now);
        }
        let rep = pass(&r, &limits(30.0, 0), now, false);
        assert_eq!((rep.calls, rep.files, rep.bytes), (2, 4, 200));
        assert_eq!(rep.why, ["older than the age limit"]);
        assert_eq!(left(&r).len(), 1, "{:?}", left(&r));
    }

    #[test]
    fn size_removes_the_oldest_until_it_fits() {
        let (r, now) = (root("size"), SystemTime::now());
        for (tg, age_h) in [(1, 30), (2, 20), (3, 10), (4, 5)] {
            call(&r, tg, age_h, 100, now);
        }
        // 400 bytes down to 250: the oldest two go (the next would be 300: still over by 50).
        let rep = pass(&r, &limits(0.0, 250), now, false);
        assert_eq!(rep.calls, 2, "{rep:?}");
        assert_eq!(rep.why, ["over the size limit"]);
        assert_eq!(left(&r), ["3-1000010_851.0.wav", "4-1000005_851.0.wav"]);
        // Exactly at the limit is fine.
        let rep = pass(&r, &limits(0.0, 200), now, false);
        assert_eq!(rep.calls, 0);
    }

    #[test]
    fn the_smaller_of_the_size_and_the_share_of_the_disk() {
        let cfg = |gb, pct| Config { max_size_gb: gb, max_disk_pct: pct, ..Default::default() };
        let gb = GB as u64;
        // 10% of a 20 GB disk is 2 GB; the size limit says 3.
        assert_eq!(Limits::new(&cfg(3.0, 10.0), Some(20 * gb)).bytes, Some(2 * gb));
        // 50% of it is 10 GB; the size limit says 3.
        assert_eq!(Limits::new(&cfg(3.0, 50.0), Some(20 * gb)).bytes, Some(3 * gb));
        // A share of a disk of unknown size does nothing.
        assert_eq!(Limits::new(&cfg(0.0, 10.0), None).bytes, None);
        // An age too big to be a duration is no limit.
        assert_eq!(Limits::new(&Config { max_age_days: 1e308, ..Default::default() }, None).age, None);
    }

    #[test]
    fn age_and_size_together_count_only_what_is_left() {
        let (r, now) = (root("both"), SystemTime::now());
        for (tg, age_h) in [(1, 80 * 24), (2, 10), (3, 5)] {
            call(&r, tg, age_h, 100, now);
        }
        // Age removes the first; the remaining 200 bytes fit in 250.
        let rep = pass(&r, &limits(2.0, 250), now, false);
        assert_eq!(rep.calls, 1, "{rep:?}");
        assert_eq!(left(&r).len(), 2);
    }

    #[test]
    fn a_recent_call_is_never_removed() {
        let (r, now) = (root("recent"), SystemTime::now());
        call(&r, 1, 5, 100, now);
        // One 30 seconds old: though the folder is still over, it stays.
        let p = r.join("2-1000000_851.0.wav");
        write(&p, 100);
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(now - Duration::from_secs(30)).unwrap();
        let rep = pass(&r, &limits(0.0, 50), now, false);
        assert_eq!(rep.calls, 1);
        assert_eq!(left(&r), ["2-1000000_851.0.wav"]);
        let rep = pass(&r, &limits(0.000001, 0), now, false);
        assert_eq!(rep.calls, 0, "an age of a second removes nothing that is 30 seconds old");
    }

    #[test]
    fn a_pass_deletes_whole_calls_and_only_calls() {
        let (r, now) = (root("whole"), SystemTime::now());
        let old = r.join("a/2026/01/01");
        let new = r.join("a/2026/10/13");
        for ext in ["wav", "json", "m4a", "frames.jsonl", "sdr"] {
            let p = old.join(format!("5-100_851.0.{ext}"));
            write(&p, 10);
            set_age(&p, now, 24 * 60);
        }
        write(&old.join("notes.txt"), 10);
        set_age(&old.join("notes.txt"), now, 24 * 60);
        for ext in ["wav", "json"] {
            let p = new.join(format!("5-200_851.0.{ext}"));
            write(&p, 10);
            set_age(&p, now, 2);
        }
        let rep = pass(&r, &limits(30.0, 0), now, false);
        assert_eq!((rep.calls, rep.files, rep.bytes), (1, 5, 50));
        assert!(old.join("notes.txt").exists(), "other files stay, and so does their folder");
        assert!(!old.join("5-100_851.0.wav").exists() && !old.join("5-100_851.0.json").exists());
        assert!(new.join("5-200_851.0.wav").exists() && new.join("5-200_851.0.json").exists());
        // Delete the note: the folders that held only the old call go on the next pass.
        fs::remove_file(old.join("notes.txt")).unwrap();
        write(&old.join("5-100_851.0.wav"), 10);
        set_age(&old.join("5-100_851.0.wav"), now, 24 * 60);
        pass(&r, &limits(30.0, 0), now, false);
        assert!(!r.join("a/2026/01").exists(), "emptied folders go");
        assert!(r.exists() && new.exists(), "the recordings folder and busy ones stay");
    }

    #[test]
    fn only_a_calls_own_files_are_calls() {
        assert!(is_call_name("5-1791952226_851012500") && is_call_name("1125-1791952226_852650000.1") && is_call_name("5-1_2.0.1"));
        for no in ["notes", "audio", "5-1791952226", "-1_2", "a-1_2", "5-1_", "5-1_2.x", "5-1_2."] {
            assert!(!is_call_name(no), "{no}");
        }
        let (r, now) = (root("id"), SystemTime::now());
        let call_json = br#"{"talkgroup":5,"start_time":1,"call_length":3,"freq":851000000}"#;
        // A custom-named call, known by its JSON; stray files with call-like endings.
        for (n, body) in
            [("kcpd-dispatch.json", &call_json[..]), ("kcpd-dispatch.wav", b"x"), ("notes.json", br#"{"a":1}"#), ("audio.wav", b"x"), ("list.json", b"[1,2]")]
        {
            fs::write(r.join(n), body).unwrap();
            set_age(&r.join(n), now, 24 * 90);
        }
        let rep = pass(&r, &limits(30.0, 0), now, false);
        assert_eq!((rep.calls, rep.files), (1, 2));
        assert!(!r.join("kcpd-dispatch.wav").exists() && !r.join("kcpd-dispatch.json").exists());
        for keep in ["notes.json", "audio.wav", "list.json"] {
            assert!(r.join(keep).exists(), "{keep}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_followed() {
        let (r, outside, now) = (root("links"), root("links-outside"), SystemTime::now());
        write(&outside.join("5-100_851.0.wav"), 10);
        set_age(&outside.join("5-100_851.0.wav"), now, 24 * 90);
        std::os::unix::fs::symlink(&outside, r.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(outside.join("5-100_851.0.wav"), r.join("5-200_851.0.wav")).unwrap();
        let rep = pass(&r, &limits(30.0, 0), now, false);
        assert_eq!((rep.calls, rep.files), (0, 0));
        assert!(outside.join("5-100_851.0.wav").exists(), "a call outside the folder, reached through a link, stays");
    }

    #[test]
    fn a_dry_run_deletes_nothing_and_says_what_would_go() {
        let (r, now) = (root("dry"), SystemTime::now());
        call(&r, 1, 80 * 24, 100, now);
        call(&r, 2, 1, 100, now);
        let rep = pass(&r, &limits(30.0, 0), now, true);
        assert_eq!((rep.calls, rep.files, rep.bytes), (1, 2, 100));
        assert_eq!(left(&r).len(), 2, "both are still there");
        let rep = pass(&r, &limits(30.0, 0), now, false);
        assert_eq!((rep.calls, rep.files), (1, 2));
        assert_eq!(left(&r).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_deleted_is_reported_and_the_rest_go_on() {
        use std::os::unix::fs::PermissionsExt;
        let (r, now) = (root("failed"), SystemTime::now());
        let stuck = r.join("stuck");
        call(&stuck, 1, 80 * 24, 100, now);
        call(&r, 2, 80 * 24, 100, now);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        // (A user who can write there anyway, root, has nothing to test.)
        if fs::File::create(stuck.join("probe")).is_ok() {
            fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let rep = pass(&r, &limits(30.0, 0), now, false);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(rep.failed.len(), 2, "{rep:?}");
        assert_eq!((rep.calls, rep.files), (1, 2), "the other call went");
        assert_eq!(left(&stuck).len(), 1);
    }

    #[test]
    fn settings_are_checked() {
        let c = |d, g, p| Config { max_age_days: d, max_size_gb: g, max_disk_pct: p, dry_run: false };
        assert!(c(0.0, 0.0, 0.0).check().is_ok());
        assert!(c(90.0, 0.0, 80.0).check().is_ok());
        assert!(c(-1.0, 0.0, 0.0).check().is_err());
        assert!(c(0.0, f64::NAN, 0.0).check().is_err());
        assert!(c(0.0, 0.0, 96.0).check().is_err());
        assert!(c(0.0, 0.0, 0.5).check().is_err());
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

    #[test]
    fn a_root_or_missing_folder_is_refused() {
        for folder in ["/", "", "/no/such/folder/for/retention"] {
            let dir = testing::temp_dir("retention");
            let mut hello = testing::hello(&dir, json!({ "maxAgeDays": 1 }));
            hello.capture_dir = PathBuf::from(folder);
            let out = testing::run::<Retention>([HostMessage::Hello(hello)]);
            assert!(!out.ready(), "{folder:?} was accepted");
            assert_eq!(out.exit_code, trunk_recorder_plugin::EXIT_CONFIG, "{folder:?}");
        }
    }
}
