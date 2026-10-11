Reliability changes

- Album metadata parsing retries three times instead of panicking on missing markers or invalid JSON5.
- HTTP request loops have three attempts with 2/4-second backoff. Connection timeout is 15 seconds; read timeout is 30 seconds. Album/status/job control requests also have a 45-second total timeout.
- A missing audio job returns to job creation instead of repeatedly requesting the expired download URL. At most three job lifecycles are attempted per track, with five seconds between jobs. Processing has a three-minute deadline per job; unchanged status still restarts after 30 seconds.
- Explicit "not available for streaming", "unavailable for streaming", and "not streamable" errors skip tracks. Processing statuses unavailable, error, failed, and cancelled are handled. These messages are conservative heuristics; unknown server failures exhaust retries and are reported as failures.
- Bounded stream channels prevent unlimited queued audio. Files are flushed before renaming; only an explicit successful end-of-stream permits promotion. Failed transfers preserve existing completed files.
- Ctrl-C cancels active album/track futures. File errors are logged; unexpected item panics are isolated so the remaining queue can continue.
- The end-of-run report counts downloaded, already-present and unavailable tracks, and lists failed albums, tracks and covers with reasons, grouped by album directory. It is saved as lucida-report-*.txt when anything needs attention. Failed/interrupted URLs (album URLs for failed covers) are written to lucida-failed-*.txt, suitable for -f. A batch with failures exits unsuccessfully.
- Partial .part files are removed when a transfer fails, the retry budget is exhausted, or the download is cancelled.
- A standalone track already saved under an inferred number is recognized as downloaded, so retry lists do not download it again.
- Blank URL-list lines are ignored; zero workers and invalid startup inputs are rejected.

Validation: regression tests cover changed metadata markers, explicit unavailability classification, missing HTTP jobs, and buffered-file completion/interruption. cargo clippy --all-targets -- -D warnings and optimized build pass. No authenticated live Lucida downloads were used for validation.
