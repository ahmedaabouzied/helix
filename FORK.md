# Fork notes

A personal fork of [helix-editor/helix](https://github.com/helix-editor/helix).
Everything here is local to this fork; upstream knows nothing about it. The
layout below exists so that merging an upstream release stays routine.

## The rule

Customizations live in their own files. Upstream files get the smallest
possible hook — ideally a single call into fork-owned code — wrapped in
sentinel comments:

```rust
// ====== fork: <feature> (begin) ======
...
// ====== fork: <feature> (end) ======
```

Git never produces a conflict in a file only this fork has, so the maintenance
cost of a customization is proportional to its hook, not to its size.

## Customizations

### Welcome screen

A dashboard shown when `hx` starts with no file arguments: banner, a menu of
actions with hotkeys, `j`/`k` or arrows to move, Enter to run. Any other key
dismisses it and falls through to the editor. Pickers opened from it are
layered on top, so dismissing a picker returns to the screen; once a document
is actually open the layer stops drawing and retires itself.

Owned files:

| File | Contents |
| --- | --- |
| `helix-term/src/welcome/mod.rs` | the compositor layer: layout, keys, actions |
| `helix-term/src/welcome/config.rs` | `welcome.toml` |

Upstream files touched — the complete list, nine lines:

| File | Change |
| --- | --- |
| `helix-term/src/lib.rs` | `pub mod welcome;` |
| `helix-term/src/application.rs` | `welcome::layer(&args)` in `Application::new` |

`welcome::layer` is the only entry point: it decides whether the screen should
appear, loads the config and builds the component. Extending the feature never
widens the footprint above.

Configuration lives in `~/.config/helix/welcome.toml`, separate from
`config.toml` because Helix's `ConfigRaw` is `deny_unknown_fields` — the module
docs in `config.rs` explain the trade.

```toml
enable = true
banner = ["  my own", "  ascii art"]  # omit for the built-in art, [] to hide
footer = "carpe diem"                 # omit for `helix <version>`, "" to hide
```

The menu itself is Rust, in `ITEMS` and `Action::run`.

## Merging upstream

One-time setup:

```sh
git remote add upstream https://github.com/helix-editor/helix
git config rerere.enabled true
```

`rerere` records how a conflict was resolved and replays that resolution the
next time the same one shows up — which is exactly what happens when the same
hook lines conflict release after release.

Per release:

```sh
git fetch upstream --tags
git rebase upstream/master      # or a release tag, e.g. `git rebase 25.10`
cargo clippy --workspace        # upstream CI runs with -D warnings
cargo test --workspace
```

Rebasing rather than merging keeps the fork as a readable patch series on top
of upstream, which keeps these useful:

```sh
git log --oneline upstream/master..                            # every customization
git diff upstream/master --stat -- ':!helix-term/src/welcome'  # the hook footprint
```

That second number should stay small. If it grows, a customization has leaked
out of its own files and is worth pulling back in.

Keep one commit per customization, so a rebase conflicts in at most that one
commit. Force-pushing to `origin` after a rebase is fine for a single-user
fork; if this is ever shared, switch to `git merge <tag>` instead.
