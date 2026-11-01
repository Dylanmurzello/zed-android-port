# Screenshot / video shooting list

Filenames the README references and what each one should show. Goal:
**give people the "wait, on a tablet?" surprise per asset.** Each one
should be visibly Android (status bar / soft keys / DeX chrome / tablet
proportions) so it's instantly clear this isn't the desktop app.

All assets land under `crates/gpui_android/examples/zed_android/docs/screenshots/`.

## `hero.gif`

The single most important asset. Lands at the top of the README,
above the fold on GitHub. People decide whether to scroll based on this.

**Length:** 45–60 seconds. Loop seamlessly.

**Shoot list:**

1. **Cold-start the app** (`adb shell am force-stop com.zdroid &&
   adb shell am start -n com.zdroid/.MainActivity`). Let the welcome /
   workspace appear. ~3s.
2. **Open a real project** with non-trivial structure — the Zed source
   itself, or any meaty Rust/Go/TS repo. File tree
   visible on the right. ~2s.
3. **Open a `.rs` file.** Cursor in editor. Wait ~1s for syntax
   highlighting + rust-analyzer to attach (the bottom-right LSP indicator
   should show "rust-analyzer • Connected"). ~3s.
4. **Use cmd-shift-F → search "android".** Show results streaming in. ~5s.
5. **Open a 2nd file via cmd-P / file finder** in another pane. Show
   multi-pane editing with two files visible. ~3s.
6. **Open the integrated terminal** (toggle at the bottom). Type
   `claude --version` → see `2.1.131 (Claude Code)`. Type `claude` → see
   the welcome banner. Type a quick prompt. ~10s.
7. **Switch to the git graph** (settings → "Git: File History" or via
   the git panel). Scroll the commit history. ~5s.
8. **Open extensions pane** (settings menu → Extensions). Show the
   browse list with 50+ extensions. Filter by "Themes". ~5s.
9. **Tap a theme to install / preview.** Instant visual change. ~3s.
10. **Open SSH server picker** (title bar pill). Show saved servers
    list. ~3s.
11. **Final beat:** quick wide shot of the workspace fully populated
    with a project + terminal + git panel visible. Hold ~2s before loop.

**Capture notes:**
- Use `adb exec-out screenrecord --bit-rate 8000000 /sdcard/hero.mp4`
  for raw capture, then `ffmpeg -i hero.mp4 -vf "fps=20,scale=1600:-1" -loop 0 hero.gif`
  for the GIF (or palette-optimized version with `gifski`).
- 1600px wide is enough; GitHub renders inline at ~860px, full-res on
  click. GIF should land < 12 MB or GitHub may transcode poorly.
- If the GIF gets too heavy, ship a poster image + link to MP4 on the
  releases page.

## `workspace.png`

Headline static screenshot — the "this is real Zed" beat.

**Compose:**
- Tab S9 Ultra in landscape, real project open (something with depth in
  the file tree — Zed source itself works great).
- Project panel left, two editor panes right (rust + markdown or rust
  + toml), tab strip visible.
- Status bar bottom showing language + branch + LSP active dot.
- One symbol on screen with an active hover popover (definition
  preview) to make the LSP visible.
- Title bar visible — both the app menu bar and the project / SSH /
  settings chevron.

**Resolution:** 2960x1848 native, then downscale to ~1600px wide for the
README. Drop to 75-85% PNG quality so the file lands under 1 MB.

## `terminal.png`

The "a real Linux userland is INSIDE the editor and works" beat.

**Compose:**
- Editor pane on top half, integrated terminal on the bottom half.
- Terminal showing a multi-line scrollback proof-of-realness:
  ```
  $ apt install -y golang-go nodejs npm
  $ go version
  go version go1.26.2 linux/arm64
  $ npm install -g @anthropic-ai/claude-code
  added 1 package in 4.2s
  $ claude --version
  2.1.131 (Claude Code)
  $ ssh user@your-vultr.example.com
  Last login: ...
  ```
  Doesn't have to be live; even a static screenshot of the scrollback is
  enough. The point is "real native binaries running, no SSH bridge."

## `git_graph.png`

The "real Zed features, not a stripped-down port" beat.

**Compose:**
- Git graph view active — full commit DAG, lanes, hashes, dates,
  authors visible. Pick a project with at least 50 commits and some
  branch merges so the graph has visible structure.
- Optionally a commit selected in the list with the diff/details
  preview pane on the right.

## `extensions.png`

The "extensions browse + install works" beat.

**Compose:**
- Extensions pane open, search bar at top, list of available extensions
  visible (themes, language packs, etc.). Show ~8-10 entries.
- Optionally one extension showing as installed (green checkmark) — the
  `html` extension auto-installs at boot, that's an easy one to feature.

## Optional bonus assets

- **`remote_ssh.png`** — title bar pill expanded into the Remote
  Projects popover, list of saved servers visible. Tells the
  "this is full SSH dev" story.
- **`vim_mode.png`** — editor in NORMAL mode (modeline visible), with a
  visual selection or a `:` command line. Shows vim is real, not a
  shim.
- **`themes_grid.png`** — 4-up of the same code in 4 different installed
  themes. Demonstrates the theme system works.
- **`dex_mode.png`** — Tab S9 in DeX (or any desktop-windowing) mode
  with Zed in a freeform window next to a browser / file manager.
  Shows the multi-window / OS-chromed extra-windows feature.

## Naming + format

- All static screenshots: PNG, 1600-2000px wide, < 1 MB each.
- All animations: GIF preferred for inline embed (< 12 MB), MP4 link
  for full quality.
- Don't include personal info in screenshots — close any auth panels,
  blank out api keys, etc. Use a fresh "Welcome" or test project for
  shoots.
