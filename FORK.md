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

The menu itself is Rust, in `ITEMS` and `Action::run`. The banner in
`welcome.toml` must have every line padded to the same width — the screen
centers each line independently, so ragged lines scatter the art.

### Binary name

The fork installs as `dhx` — Double Helix — so it never collides with an `hx`
already on PATH and needs no alias to coexist with one.

| File | Change |
| --- | --- |
| `helix-term/Cargo.toml` | `default-run` and `[[bin]] name` are `dhx` |
| `helix-term/src/main.rs` | the `USAGE:` line printed by `--help` |

Install:

```sh
HELIX_DEFAULT_RUNTIME="$PWD/runtime" cargo install --path helix-term --locked
```

The env var is read with `option_env!`, so the path is compiled into the binary.
That part is not optional: outside `cargo run` there is no `CARGO_MANIFEST_DIR`,
so the loader only checks `~/.config/helix/runtime` and the directory beside the
executable — an installed `dhx` would otherwise start with no grammars, no
themes and no tutor.

Do not symlink `~/.config/helix/runtime` at the repo instead. That path outranks
the baked one *and* is shared with every other Helix on the machine, so an older
`hx` would pick up this fork's grammars and fail to load them across the
tree-sitter ABI gap.

### Picker preview layout

`editor.picker-preview` chooses where the picker draws its preview: `right`
(upstream's layout, the default) or `bottom`. Upstream always splits left/right,
which halves the width a second time when Helix is itself in a vertical split —
list and preview each end up a quarter of the screen wide.

```toml
[editor]
picker-preview = "bottom"
```

Owned files:

| File | Contents |
| --- | --- |
| `helix-view/src/fork/mod.rs` | fork-local additions to `helix-view` |
| `helix-view/src/fork/picker_preview.rs` | the `PickerPreview` enum, the split, and its tests |

Upstream files touched:

| File | Change |
| --- | --- |
| `helix-view/src/lib.rs` | `pub mod fork;` |
| `helix-view/src/editor.rs` | one `Config` field and its default |
| `helix-term/src/ui/picker.rs` | a `layout` helper, called from `render` and `cursor` |

`render` and `cursor` previously carried the same layout arithmetic twice;
routing both through one helper means a future layout change cannot desync the
cursor from the list. `MIN_WIDTH` in the fork module duplicates the value of
`MIN_AREA_WIDTH_FOR_PREVIEW` because `helix-view` cannot depend on `helix-term`.

### File tree

`:file-tree` (aliased `:tree`) opens a modal file tree over the editor, in the
manner of a picker. Bind it with `"C-n" = ":file-tree"` under `[keys.normal]`;
no default binding is added, which is what keeps `keymap/default.rs` untouched.

| key | |
| --- | --- |
| `j` `k` `↓` `↑` `C-n` `C-p` | move, wrapping |
| `g` `G` | first row, last row |
| `C-d` `C-u` `PageUp` `PageDown` | half a page, stopping at the ends |
| `l` `Enter` `→` | expand or collapse a directory, open a file |
| `h` `←` | collapse, or step out to the parent |
| `a` `A` | create file, create directory |
| `r` | rename |
| `d` | delete, confirmed with `y` |
| `Esc` `C-c` | close |

Modal rather than a sidebar on purpose. A permanent pane costs width in every
split forever — a real loss when Helix is already sharing the screen — and it
would need `EditorView::render` to reserve space, putting a hook in the file
upstream changes most. This needs neither.

Owned files:

| File | Contents |
| --- | --- |
| `helix-term/src/file_tree/model.rs` | rows, expansion, selection, and the directory read; unit tested |
| `helix-term/src/file_tree/mod.rs` | the `Component`, its keys, and the file operations |

Rows are drawn as `indent | marker | icon | name`; the icons come from
[File-type icons](#file-type-icons) below.

Upstream files touched:

| File | Change |
| --- | --- |
| `helix-term/src/lib.rs` | `pub mod file_tree;` |
| `helix-term/src/commands/typed.rs` | one `TYPABLE_COMMAND_LIST` entry, appended at the end |

The command entry sits at the *end* of the list rather than in name order: a
neighbouring upstream edit would otherwise collide with the fork block, whereas
at the end the only conflicts are upstream's own appends, which resolve as "keep
both". `fun` points at `crate::file_tree::open`, so no fork logic lives in
`typed.rs`.

Two things worth knowing before changing this code:

- **Rows are stored flattened in display order**, not as a nested tree.
  Expanding splices children in behind their parent; collapsing drains the run
  of deeper rows that follows. Rendering and cursor movement index straight into
  that list. `expand` and `collapse` maintain the selection themselves, so a row
  vanishing from under the cursor cannot leave it pointing at the wrong file.
- **Every file operation goes through `Editor`**, never `std::fs` — `create_path`,
  `move_path`, `delete_path`. Each sends the matching `will*`/`did*` LSP
  notification, which is how a language server updates imports across the
  workspace when a file is renamed. Note `create_path` writes with `fs::write`
  and would truncate an existing file, so creation checks for one first.

The root has no row of its own, so creating or deleting directly inside it
rebuilds the listing and collapses everything. Giving the root a row would fix
that, at the cost of reworking the model and its tests.

### File-type icons

`helix_view::fork::icons` maps a path to a Nerd Font glyph and a colour:

```rust
icons::for_file(path)          // -> Icon { glyph, color }
icons::for_directory(expanded) // -> Icon
```

Generated from the `icons_by_file_extension` and `icons_by_filename` tables of
[nvim-web-devicons](https://github.com/nvim-tree/nvim-web-devicons), so glyphs
and colours match the Neovim setup this fork's theme was ported from. 212
whole-name entries, 484 by extension. Regenerate by re-parsing those Lua tables
if the plugin is updated — the file is generated and not meant to be edited by
hand.

Owned files, and no upstream files at all:

| File | Contents |
| --- | --- |
| `helix-view/src/fork/icons.rs` | the tables and the lookup |

It sits in `helix-view` rather than beside its only current caller so that
anything rendering a path can reach it — the file tree today, a statusline or
bufferline later. Registering it costs nothing, because `helix-view/src/fork/`
is itself fork-owned.

Three things to know before changing it:

- **Lookup order is whole filename, then extension, then the whole name again as
  an extension.** That last step is not redundant: devicons keys extensionless
  names like `Dockerfile` and `Makefile` in the *extension* table. Dropping it
  silently loses their icons.
- **Both tables are sorted and searched with `binary_search`.** A test guards the
  ordering, because an out-of-order edit does not fail loudly — lookups just
  start missing.
- **Icons are drawn with `style.fg(icon.color)`**, which replaces only the
  foreground, so a selected row keeps its highlight behind the glyph and the
  glyph keeps its own colour. Drop the `.fg(...)` to make icons follow the
  theme instead. `ICON_WIDTH` assumes glyphs measure one cell, as Nerd Fonts
  and nvim do; a terminal that renders them double-width needs 3.

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
