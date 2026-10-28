# Retention — a Trunk Recorder Pro plugin

Deletes the oldest calls from the recordings folder once it is older, bigger, or a bigger share of
its disk than you allow. Without it the folder grows until the disk is full, or until you clear it
by hand or with a cron job.

A [Trunk Recorder Pro](https://github.com/TrunkRecorder/trunk-recorder-pro) plugin, built on the
[plugin SDK](https://crates.io/crates/trunk-recorder-plugin).

## Settings

Each limit is off at `0`. Any that are set apply together.

| Setting | Key | What it does |
|---|---|---|
| Keep calls for, days | `maxAgeDays` | Calls older than this are deleted |
| Keep at most, GB | `maxSizeGb` | The oldest calls go while the folder holds more than this (a gigabyte is 10⁹ bytes) |
| Keep at most, % of the disk | `maxDiskPct` | The same, as a share of the disk's capacity (not of what is free), from 1 to 95 |
| Dry run | `dryRun` | Say in the log what a pass would delete, and delete nothing |

If a size and a share are both set, the smaller limit wins. A share can't be worked out when the
disk's size isn't known, and is then skipped.

```json
{ "plugins": { "retention": { "maxAgeDays": 90, "maxDiskPct": 80 } } }
```

## What it does

- A pass runs a minute after recording starts and every ten minutes after. The plugin runs only
  while the recorder is recording, so nothing is deleted while it is stopped.
- Only a call's own files go (`.wav`, `.m4a`, `.json`, `.sdr` and `.frames.jsonl`), all of a call's
  together, the oldest call first. Other files in the folder are never touched, and neither are
  hidden ones. Folders left empty are removed; the recordings folder itself stays.
- **What counts as a call:** files named as the recorder names them (`<talkgroup>-<start>_<freq>`),
  or any whose call JSON has a call's fields (`talkgroup`, `start_time`, `call_length`), which is
  how a call with its own `filenameFormat` is known. With a custom `filenameFormat` and call JSON
  off, nothing identifies a call's audio, so it is left alone.
- A call changed in the last ten minutes is never deleted: it may still be recording, or waiting
  for an upload plugin.
- A call's age is when its newest file last changed. It goes by file time, so files copied without
  keeping their times look new. A file whose time can't be read counts as just written.
- The size limits count call files only, not other data on the disk, and are met to the minute: up
  to a minute of calls more than needed can go.
- A pass walks the folder twice, a folder at a time (to find where the limit falls, then to delete),
  so its memory does not grow with the number of calls. (A dry run over about 35 GB of recordings
  finished within a minute and the plugin stood at under 10 MB afterwards; I didn't measure its peak.)
- It deletes from the recordings folder the recorder was started with. A change to `captureDir`
  applies at the next Start. Each pass that deletes anything says so in the plugin's log.
- The recorder's call list still shows calls the plugin deleted, and can't play them.

**It deletes files.** When you set or change a limit, turn on **Dry run** first: the first pass
says in the log how many calls and how many megabytes it would delete, and deletes nothing. Look
at that, then turn Dry run off.

Calls an upload plugin is still retrying are not known to it: with a short age limit, or a size
limit the folder is already well over, it can delete the files of a call an uploader has queued.
Keep calls for days, not hours, if your uploaders can be down for a while.

## Build

```sh
cargo build --release
./target/release/retention --describe
cargo test
```

To try it on a recorder, add the executable as a plugin of your own on the Plugins page, then set
its limits there. `trunk-pro plugin run` sends a plugin calls from disk, but this one watches no
calls and its first pass is a minute after start, so it deletes nothing that way.

## License

MIT
