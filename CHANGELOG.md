# Changelog

Each version's section is its GitHub release notes.

## [0.1.0] — unreleased

- First release: deletes the oldest calls from the recordings folder once it is older than `maxAgeDays`,
  bigger than `maxSizeGb`, or a bigger share of its disk than `maxDiskPct`. A pass runs a minute
  after recording starts and every ten minutes after. Only a call's own files go, all of a call's
  together, oldest first; a call changed in the last ten minutes stays.
- A dry run setting, and a pass that holds one folder's calls at a time instead of every call.
