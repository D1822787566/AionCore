# HSSF bridge

This short-lived helper reads one JSON request from stdin and returns one JSON response.
It never resolves user/session identities, chooses delivery paths, deletes files, executes
macros or downloads dependencies. The Rust file coordinator owns those boundaries.

Build on Windows x64 with the pinned JDK from `dependencies.lock.json`:

```powershell
python crates/aionui-office/hssf/build.py --jdk JDK_DIRECTORY --cache JAR_CACHE_DIRECTORY --output NEW_BUNDLE_DIRECTORY
```

The build checks dependency SHA-256 values, compiles the bridge, creates a reduced
runtime, includes dependency notices and Java legal files, and writes a per-file
manifest. Stage the result with `prepareHssf` in AionUi's `scripts/prepare-officecli.js`; app runtime
uses `AIONUI_HSSF_HOME` and never needs a system Java installation.

Actions are `inspect` (optional `sheet` plus `range`), `write` (`changes` and a fresh
`output` path), and `verify` (reopen and compare expected typed changes).
All actions require `path`. Write types are string, number, boolean, date, blank
and formula. Blank omits value. Formula omits the leading `=`. Date uses an existing
date style and either `YYYY-MM-DD` or `YYYY-MM-DDTHH:mm:ss`.

Known unaccepted compound entries, drawings other than comments, external links
and chart/pivot records block writes. Detection describes known parsed structures;
it does not claim complete recognition of every BIFF extension.
