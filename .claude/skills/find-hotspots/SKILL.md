---
name: find-hotspots
description: Profile roleforge with samply and rank the hottest functions by CPU time. Use when asked where the time goes, what is slow, to find or confirm a performance hotspot, to check whether an optimisation actually helped, or when a change needs a before/after CPU comparison rather than a guess.
---

# Finding hotspots

`samply` records a sampling profile; `hotspots.mjs` turns it into a ranked list
of function names. The list is the point — samply's own output is a browser UI
and a `profile.json.gz` full of raw hex addresses.

Paths below are relative to the repo root.

**Profile the right thing first.** `roleforge pull` is mostly blocked on HTTP,
so a naive profile ranks TLS and socket waits. The CPU that matters is the
parse/match path — `text::clean`, the regexes in `text` and `matching`,
`sources::parse_feed`, `matching::score_job` — over feed bodies already in
hand. Prefer profiling the test binary or a saved-fixture run over a live
`pull` unless the network time is itself the question.

## 1. Build with symbols

```powershell
cargo build --profile profiling
```

`[profile.profiling]` is release + `debug = 1`, so frames resolve to names
without the binary being sluggish. Output: `target/profiling/roleforge.exe`.

## 2. Record

Anything that runs to completion:

```powershell
samply record --save-only -o hot.json.gz -- ./target/profiling/roleforge.exe pull
```

The test binary works the same way, and is the deterministic option — build it
first, then record the exact executable cargo names:

```powershell
cargo test --profile profiling --no-run 2>&1 | Select-String 'Executable'
samply record --save-only -o hot.json.gz -- ./target/profiling/deps/roleforge-abc123.exe --test-threads=1
```

`--test-threads=1` keeps the self-time list attributable to one thread.

## 3. Rank

```bash
node .claude/skills/find-hotspots/hotspots.mjs hot.json.gz --module roleforge.exe --top 20
```

```
hot.json.gz — 12.4 s of CPU across 6 threads

CPU by thread
     9820 ms  79.2%  roleforge.exe (49620)
      1140 ms   9.2%  tokio-runtime-w (82652)   <- reqwest's internal pool

Self time — top 20 in roleforge.exe
      172 ms   2.3%  regex_automata::...::try_search_fwd()  (dfa.rs:1163)
      169 ms   2.3%  roleforge::text::clean()  (text.rs:14)
      ...
```

- **Self time** is where the CPU actually is — start here.
- **Total (inclusive) time** says which caller owns it. Near-100% entries at
  the top are just `main` and the thread starts.
- `--module roleforge.exe` filters by binary — without it the top of the list
  is `ntdll` wait stubs. `--thread <substr>` restricts to one thread.
- Weights are CPU microseconds, not sample counts: a network-bound run is
  mostly idle samples parked in `NtWaitForMultipleObjects`, and counting them
  would rank the parked threads first.

To compare before and after, record twice over the same input and diff the two
self-time lists. Absolute ms only compares across runs of equal length;
percentages are safer.

## Gotchas

- **Symbolicate before rebuilding.** samply stores raw addresses and resolves
  them lazily against the PDB. Rebuild the binary and the build id no longer
  matches, so every name in an older profile falls back to `17:18439557`
  forever. If that is what you see, the profile is stale — re-record.
- **System DLLs stay unresolved** unless you pass a symbol server:
  `--windows-symbol-server https://msdl.microsoft.com/download/symbols` on
  `samply load`/`record`. Rarely worth it; `--module roleforge.exe` is the
  answer to `ntdll.dll` noise.
- **`hotspots.mjs` starts `samply load` on port 3579** to reach its
  `/symbolicate/v5` endpoint, and kills it on the way out. `--port` if that
  clashes. It never opens a browser. It also passes `--symbol-dir` for every
  directory the profile loaded a module from — the profile names each PDB by
  bare filename, so without that samply looks in the cwd and resolves nothing.
- **Inlined callees are attributed to the caller.** A "missing" function in a
  release build was almost certainly inlined; look for its caller. The
  `OnceLock` regex accessors in `text.rs` inline away entirely — the time
  lands in `regex_automata`, under whichever caller ran the match.
- **A live `pull` is not reproducible.** Feed contents, response sizes and
  network latency all move between runs, so before/after numbers from two
  `pull` runs are not comparable. Use the test binary, or cache the feed
  bodies, when the difference you are measuring is smaller than the noise.
- **`samply record` wedges if a recording is orphaned.** When a record dies
  without its child, every later `samply record` hangs at startup or fails with
  `The handle is invalid` — even `samply record -- cmd /c ver`. No stale ETW
  session is visible (`logman query -ets`, `xperf -stop`, `wpr -status` are all
  clean). The fix is to kill both leftovers and retry:

  ```powershell
  Stop-Process -Name samply,roleforge -Force -ErrorAction SilentlyContinue
  samply record --save-only -o probe.json.gz -- cmd /c ver   # should print the Windows version
  ```

- **Numbers from the first run after a build are not a baseline.** A first
  measurement can come out several times slower than the same binary measured
  minutes later. Always take before/after back to back, and repeat the baseline
  once.

## Human path

```powershell
samply record ./target/profiling/roleforge.exe pull
```

Opens the Firefox Profiler UI with a flame graph, which is better than any
text list for *shape* — useless to an agent, which cannot read it.
