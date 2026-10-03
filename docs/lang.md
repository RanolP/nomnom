# nomnom rule language

A small, total language for saying what a file or directory **is**, who owns it, and what — if anything — its owner would suggest doing with it. It is not a scripting language and never becomes one: a rule can classify a path, read a bounded set of named manifest files, and pick an action from a fixed list compiled into the binary. Nothing else. That restriction is what makes it safe to load rules written by someone else.

## Why a language at all

Milestone 1 answered "what is this path" with hand-written Rust. That works exactly until someone who is not us wants to teach nomnom about a toolchain we have never heard of — and then it fails completely, because the only way to add a rule is to recompile the tool.

The product nomnom is growing into makes that failure total. nomnom is a **file classifier and inspector**: it recognizes the Steam installation and suggests the old, unplayed games in it; it recognizes Windows itself, Telegram's downloads, pnpm's global store, and every other program that lays files down in a shape it can name. Mole does this for one platform with a catalog kept in its own tree; nomnom wants it for every platform, every program and every environment. That is hundreds of programs, and each one has to be a **plugin**, never a case written into the engine.

So the rules move out of Rust and into data, and the data becomes distributable. The language has two jobs at once: writing a typical plugin should take a few lines, and the engine should be able to see enough of a rule's shape to run hundreds of packs without visiting every node once per rule. Everything below follows from those two goals.

### Why a declarative language, and not WASM or executables

Two other plugin models were considered and rejected.

- **WASM modules.** A compiled blob cannot be reviewed, so `pack trust` would become blind consent to a program that can misclassify cleverly. It would also add a runtime, an ABI to version, and a toolchain for every plugin author, and each of the hundreds of small plugins would be code instead of five lines.
- **External executables.** They have no sandbox and run with the user's full rights, which is unacceptable in a tool whose deletions are permanent. Results and CLI/GUI parity would also depend on whatever happens to be on `PATH`.

A `.nom` pack stays text a human can audit. It cannot compute, open a path it did not spell out, reach the network, or launch anything outside the handler registry. The only Rust that is not a general mechanism is a closed set of **file formats**, **known folders** and **action handlers**, and every pack reuses it. Adding to one of those sets needs a nomnom release, by design: they are the code everyone trusts.

## Status: what exists today

This document describes the language as it is and as it is being migrated, unit by unit. Every section that is not implemented says so in a **Status** line, naming the unit that delivers it. Do not write a pack or engine code against a planned construct before its unit lands: today's parser rejects every long-form keyword, and a pack using one fails to load.

| unit | delivers |
|---|---|
| implemented | the short form (`[Title]` + keys + `filter { … then … }`), kinds, the filter grammar and vocabulary, packs and their resolution order, conflicts and nesting, git-pinned packs with a lock, the trust cap, engine guards, permanent deletion with opt-in approval |
| Unit 2, ownership | every short-form target becomes an exclusive claim; the claimed/arbitrary byte split; the GUI "Recognized" and "Other files" views; `nomnom classify` and `--view`; downloads stops suggesting |
| Unit 3, long form | `classify`, `suggest … within`, `claim $g is`, `lens`, `at ~known/`, `in ~known`, `in class/vN`, `before`/`after … ago`, `exists`, `platforms`, `pack lint`, golden `fixtures/*.tree`, the `build.rs` pack enumeration; `builtin.downloads` ported to a lens |
| Unit 4, inspect | `table` (bounded manifest reads in vdf, json, toml, ini, plist), read caps, the read log, `suggest --reads`, the GUI "Files read" view |
| Unit 5 | `builtin.steam`, classify only: client, library and game claims with facts |
| Unit 6, actions | the handler registry, `action =`, `Action::Request`, the two Steam suggestions, `windows.storage-settings` |
| Unit 7, trust | `builtin.epic`; the community pack repository; trust becomes the gate to loading a pack at all |
| later, undecided | tool-command handlers (`pnpm store prune`, `Dism`) and elevated apply |

## Ownership

**Status:** Unit 2 (short-form claims, byte split, views); Unit 3 (open claims, long form).

The disk splits in two. Part of it is **recognized**: a folder some program laid down in a shape a pack can prove — Steam's install folder, a Cargo `target/`, the Windows directory. The rest is **arbitrary**: files the user put wherever they liked. The arbitrary tree must exist, because people are free to keep their files anywhere, and nomnom never decides anything there on its own. Its job is the other half: *for the file lists that programs lay out in a fixed shape, find what can be removed without being asked where to look.*

### Claims

A **claim** says "this subtree is X, and pack P owns it". It carries:

- the claimed node, and the pack and rule that claimed it;
- a **class**, written like a kind (`steam-game/v1`) but with no disposition, declared by the pack;
- an optional **identity** (`steam:app:1245620`), stable across runs and volumes, so a game is the same game wherever its library lives;
- **facts**: typed values the claim learned, from tables (see [Tables](#tables)) or from the structure it matched;
- whether it is **exclusive** or **open**.

Claims never conflict with suggestions; a claim decides who may speak about a subtree, and a suggestion is what that owner says.

### Who owns a node

Catalog ids are in preorder, so two claims are either disjoint or nested — never partially overlapping. A node's **owner is the innermost claim that contains it**, found by binary search over the claim ranges.

- **Same node, two claims:** higher confidence wins, then the later pack, then the earlier rule — the same order as [Conflicts](#conflicts). The loser is logged.
- **Nested inside another pack's exclusive claim:** dropped, and logged. An exclusive claim means "nothing in here belongs to anyone else": once `builtin.steam` claims a game folder, no `cache` or `build-output` rule from another pack can fire on a folder inside the game that happens to look like one.
- **Nested inside the same pack's exclusive claim:** kept. The default Steam library sits inside the Steam client folder, and WinSxS sits inside the Windows directory; a pack may describe its own structure in layers.
- **Nested inside an open claim:** kept, and the innermost claim owns the node.

A node no claim contains is **arbitrary**. **The arbitrary tree gets no suggestions.** It is shown in the "Other files" view, where the user browses, sorts and picks by hand, and a lens may highlight part of it (see [Lenses](#lenses)); but nothing there is ever proposed for removal by a pack.

### The byte split

One reverse-preorder pass computes, for every node, how much of it is claimed: `claimed(n) = subtree_size(n)` when `n` is a claim root, otherwise the sum of its children's `claimed`. Then `arbitrary(n) = subtree_size(n) − claimed(n)`. Hard links follow the catalog's existing rule: a file's bytes count under its primary name, and what a suggestion can free is already reduced when the file keeps other names elsewhere, so a pnpm store full of hard links stays honest.

The GUI tree gets two roots: **Recognized** (pack › claim, with its size and facts — "Steam › Elden Ring · 48 GB · last played 412 days ago") and **Other files** (sized by arbitrary bytes, with one link row standing in for each claimed folder). The CLI says the same with `nomnom classify` and `nomnom scan --view recognized|other|all`. CLI/GUI parity is absolute: every view, filter and fact one of them shows, the other shows too.

### Evidence: every rule names its tool

There is no generic pack. A rule that matches `build/` or `cache/` wherever it appears will one day match somebody's `build/` that is not regenerable, and deletion is permanent. So **every rule names the tool that creates the files and matches on that tool's evidence**. Evidence is one of two things:

1. **A signature**: a file the tool itself writes inside or beside the target. Cargo writes `.rustc_info.json` and `CACHEDIR.TAG` into the root of every target directory, whatever it is named; pnpm writes `node_modules/.modules.yaml`; a Python venv holds `pyvenv.cfg`. A signature is the preferred evidence, because the name of a directory is never proof of what it holds.
2. **A documented path**: a path shape or name, allowed **only** when the tool's official documentation states that path explicitly and its default rarely changes. A rule that matches by name alone cites that documentation URL in a comment above it, so a reviewer can check the claim against the source.

Rules for tools that could meet on one directory exclude each other's signatures (`$nm lacks .modules.yaml | .pnpm` in the Yarn rule), so the order among built-in packs decides nothing. `pack lint` enforces the shape of this — every rule has positive evidence beyond a bare name, or carries a URL — though only a reviewer can confirm the URL says what the rule claims.

## The short form

**Status:** implemented. The claim it compiles to arrives in Unit 2.

Most plugins are a cache directory or a build directory with a signature. They stay a few lines:

```
[Yarn node_modules/]
description = regenerable: Yarn dependency tree, rebuilt by `yarn install` — it holds Yarn's `{$marker}`
kind = build-output/v1
confidence = 0.95
filter {
  $nm.dir.name == "node_modules"
  $nm has .yarn-integrity | .yarn-state.yml as $marker
  $nm lacks .modules.yaml | .pnpm | .pnpm-workspace-state-v1.json
  then $nm/
}

[Cargo target directory]
description = regenerable: Cargo build output, rebuilt by `cargo build` — it holds Cargo's `.rustc_info.json` and `CACHEDIR.TAG`
kind = build-output/v1
confidence = 0.95
filter {
  $t has .rustc_info.json
  $t has CACHEDIR.TAG
  then $t/
}
```

A rule opens with a `[Title]` line. The title is the rule's name: the CLI and the GUI show it beside every verdict the rule produces, as `pack [Title]`, and a pack name plus a title is the stable identity a verdict carries. Keys follow as `key = value`, one per line, and the rule ends with exactly one `filter { ... }` block. `#` starts a comment line.

From Unit 2, a short-form rule compiles to two things: an **exclusive claim** on its target, with class `<pack>:<kind>`, and **one suggestion** covering that whole claim. Every existing pack stays valid unchanged. Open (non-exclusive) claims are available only through the long form, because a short-form rule targets exactly the folder it proposes to remove, and nothing else should speak about the inside of that folder.

### Keys

| key | required | meaning |
|---|---|---|
| `description` | yes | the sentence a human reads before approving. A template; see below. |
| `kind` | yes | what the path is, as `name/vN`. The kind supplies the default disposition and confidence. |
| `confidence` | no, default the kind's | `0.0`–`1.0` |
| `disposition` | no, default the kind's | `keep`, `review` or `reclaimable`, and only ever a downgrade of the kind's default. A `cache/v1` rule may say `review`; a `stale-download/v1` rule may not say `reclaimable`. |

An unknown key is an error, with a "did you mean" when one is close. So is a key set twice, a missing required key, and a `confidence` outside the range.

### Kinds

| kind | disposition | confidence |
|---|---|---|
| `build-output/v1` | `reclaimable` | 0.9 |
| `cache/v1` | `reclaimable` | 0.6 |
| `stale-download/v1` | `review` | 0.5 |

A kind is versioned so its meaning can change without silently changing every rule written against the old one: a `build-output/v2` would be a new row, and rules naming `v1` keep the `v1` defaults. A pack declares kinds of its own in `pack.toml` (see [Packs](#packs)); the built-in names are reserved.

### `description`

`description` is required and must be non-empty. It is the single load-bearing field in the product: it is what a human approves a deletion on, and from milestone 3 it is what a model writes. A rule whose description restates its kind (`this is build output`) is a broken rule — the description names the evidence (`` `Cargo.toml` sits beside it ``).

A description is a template, and two kinds of hole are substitutable:

- `{field}` interpolates any vocabulary field, read from the node the verdict lands on. A `size` renders as a bare byte count and a duration as a bare whole-day count, leaving the rule to supply the word.
- `{$name}` interpolates a name the filter bound: the filter's own variable renders the name of the node it matched, and a capture (`has … as $marker`) renders the child that satisfied it, in its on-disk spelling. From Unit 4, `{$row.field}` renders a table fact (see [Tables](#tables)), and a time renders as a bare whole-day count of how long ago it was.

`{{` and `}}` are literal braces. Nothing else is substitutable, so a description cannot compute and cannot smuggle in a second expression language. An unknown field or an unbound `$name` inside braces is a parse error, not an empty substitution — a description with a hole in it is shown to a human about to delete something.

## Filters

**Status:** implemented, except the constraints marked Unit 3 or Unit 4 below.

A filter is a list of constraints, one per line, on one node — the variable every constraint names — followed by one `then` line that says which node the verdict lands on, relative to it. All constraints must hold; there is no `or` between lines.

```
$v has A | B as $m     some child of $v is named A or B; $m names the one found
$v lacks A | B         no child of $v is named A or B
$v under Name/         some strict ancestor of $v is named Name
$v.field               a bool field holds
not $v.field           a bool field does not hold
$v.field >= 90d        a field compared with a literal: == != < > <= >=
then $v/a/b/           the verdict lands on $v's child a, then its child b
```

Planned constraints, each specified in its own section below:

```
$v at ~downloads/X/          Unit 3: $v is exactly that path under an OS-resolved known folder
$v in ~downloads             Unit 3: $v is strictly inside an OS-resolved known folder
$v in telegram-downloads/v1  Unit 3: $v is strictly inside a claim of that class
exists ~app-data/X/Y/        Unit 3: that path under a known folder exists
claim $g is steam-game/v1    Unit 3: binds $g to a claim of that class (suggest only)
$v.modified before 2mo ago   Unit 3: a time field compared with a moment relative to the scan
row $r in table where …      Unit 4: a row of a table matches; its fields become facts
with $r in table where …     Unit 4: an optional row; its absence is a fact, not a failure
```

A name in `has`, `lacks` or a `then` path is one file-name component. It is compared literally when it is a plain name and as a glob when it holds `*`, `?`, `[` or `{` — `has *.csproj | *.sln as $marker`. When several children satisfy a capturing `has`, the capture reports one that matches the first alternative written, and among those the smallest name, so a description is the same on every run.

`then $v` targets the variable's own node; `then $v/target/` targets its child `target`. A trailing `/` means the target must be a directory. The scan root is never a target: it is the fence the whole plan sits inside.

`under Name/` matches **any** ancestor with that name, which is exactly why it is weak evidence: FL Studio keeps `Documents\Image-Line\Downloads`, and `$f under Downloads/` matches every file in it. Use `in ~downloads` (Unit 3) for "inside the user's Downloads folder"; `under` stays for structure inside a tool's own tree, and `pack lint` warns when its name is also a known folder's usual name.

### Each target is one decision

A target is **one** decision for its whole subtree, not one per file inside it. `node_modules` is the motivating case: 40,000 files, one verdict, and the reclaimable total counts its rolled-up size once. Nothing inside a target is judged again by a rule: when one rule's target sits inside another's, the outer target swallows the inner one.

### Shapes this makes reachable

Because the target is a path from the matched node rather than the matched node itself, these shapes are ordinary rules:

- **Match `X`, target a descendant of it.** `$p has .next / then $p/.next/cache/` takes `.next/cache` and leaves the rest of `.next` alone.
- **Match the target by what it holds.** `$nm has .yarn-integrity / then $nm/` makes the anchor the target, so the evidence is a file the tool wrote inside it. This is the shape the built-in packs prefer: a directory name is never evidence, and a manifest beside a directory says nothing about what the directory holds.
- **Match `X`, target a sibling.** `$p has *.csproj as $project / then $p/bin/` takes the `bin/` that a .NET project file beside it vouches for.
- **Match `X`, spare named children.** `then $p/.cargo/registry/cache/` takes the registry's download cache and never touches `src`, `index` or `git`, which are its siblings.

## Vocabulary

**Fields**, written `$v.field`:

```
name          file-name component
ext           extension without the dot
path          full path
size          own size in bytes
subtree_size  rolled-up size, inclusive
file_count    files in subtree
dir_count     dirs in subtree
depth         distance from scan root
is_dir  is_file  is_symlink

modified_age        how long ago this node was modified; absent with no mtime
accessed_age        how long ago it was opened; absent with no atime
has_accessed        whether the filesystem reported an atime at all
max_descendant_age  how long ago the newest file anywhere in the subtree was modified

modified  accessed  Unit 3: the same two clocks as times, for `before`/`after … ago`
```

`max_descendant_age` exists because a directory's own mtime reflects only its entry list. `node_modules` whose contents are compiled against daily still has a months-old mtime the moment nothing is added or removed from its top level, so `$d.modified_age >= 90d` on a directory measures almost nothing. The rolled-up figure is what "nobody is using this project" actually means, and it costs nothing — it rolls up in the same bottom-up pass as `subtree_size`.

Name comparison follows the platform: case-insensitive on Windows, case-sensitive elsewhere. This mirrors what the filesystem itself does — `Node_Modules` is the same directory on NTFS and a different one on ext4.

**Literals**

```
"string"                 true  false
0.95                     42
100kb  10mib  2gb        sizes, binary and decimal both understood
30d  6mo  1y  12h        durations
2mo ago                  Unit 3: a moment, that long before the scan started
```

A comparison is type-checked when the rule is parsed: `$f.size >= 90d` is an error, and so is ordering a string or a bool. There is no assignment, no loop, no function definition, no recursion: every rule terminates, and its cost is bounded before it runs.

### Relative time

**Status:** Unit 3.

"Last modified more than two months ago" is the most common question a lens or a suggestion asks, and `$f.modified_age >= 60d` makes the author do the inversion in their head: the bigger the age, the older the file. The time form reads the way the sentence does:

```
$f.modified before 2mo ago      modified more than two months ago
$f.accessed after 7d ago        opened within the last week
```

- `modified` and `accessed` are fields of type **time**. A time can only be compared with `before` or `after` against a moment literal `<duration> ago`. `==`, `<` and friends are type errors on a time, because equality to the second means nothing on a filesystem and `<` on times invites exactly the direction mistake this form exists to remove.
- `ago` is measured from the moment the scan started, not from evaluation time, so one run judges every node against the same clock.
- `X before D ago` means `X_age >= D`; `X after D ago` means `X_age < D`. The boundary is inclusive on the old side, the same as every existing `>= 90d` rule.
- Both are **false when the time is absent**. `not` applies only to bool fields, never to a comparison, so there is no way to write a constraint that holds *because* a time is unknown.
- Table facts of type time (Steam's `LastPlayed`, Unit 4) use the same form: `$g.last_played before 180d ago`.

### Absent facts

Some facts a node simply cannot supply: `accessed_age` where Windows has last-access updates turned off, `max_descendant_age` where the walk was truncated by a permission error, a table fact whose manifest hit a cap. The fact is **absent**, and every comparison against an absent fact is false.

That makes `not $f.has_accessed` the way to ask about absence, and it makes the safe direction the default one. `$f.accessed_age >= 1y` is false when the atime is unknown, so a rule that deletes on staleness stays silent rather than firing on a file it knows nothing about. The general invariant, which the engine holds and rules cannot opt out of: **unknown means keep.** Every fail-open probe in the tool we studied this design against became a data-loss incident.

`not` of an absent bool must be **false** too. Today's evaluator negates the result of the test, so `not $f.b` on an absent `b` is true; no implemented field can be absent and bool at once, so nothing fires wrongly yet. Unit 4 introduces the first absent bools (`played_found` below) and must change the evaluator, with a test, before any pack uses them.

## Known folders

**Status:** Unit 3.

A rule that means "the user's Downloads folder" must not match a folder that is merely *named* Downloads. Known folders are anchors the **operating system** resolves, never folder names the scan happens to meet.

```
$d at ~downloads/Telegram Desktop/    $d is exactly that folder
$f in ~downloads                      $f is anywhere strictly inside Downloads
exists ~app-data/Telegram Desktop/tdata/
```

- `~name` resolves through the platform's own API: on Windows, the Known Folder API (`SHGetKnownFolderPath`, so a Downloads folder the user moved to `D:\Stuff` is still `~downloads`); on macOS, the system's standard directories; on Linux, the XDG user directories and base-directory variables.
- The vocabulary is closed and lives in the language crate: `~home`, `~downloads`, `~documents`, `~app-data` (roaming), `~local-app-data`, `~cache` (`%LocalAppData%` / `~/Library/Caches` / `$XDG_CACHE_HOME`), `~temp`, `~windows`, `~program-files`, `~program-data`, `~system-drive`. A name outside it is a parse error with a "did you mean". Adding one needs a nomnom release.
- v1 resolves the current user's folders only. A folder that does not resolve on this machine, or resolves outside the scan, makes every constraint naming it false: unknown means keep.
- Segments after the known folder are literal file-name components; globs are refused, because the point of an anchor is that it names one place.
- `at` means *is exactly this node*; `in` means *strictly inside*, so `$f in ~downloads` never matches the Downloads folder itself. `in` also takes a claim class (`$f in telegram-downloads/v1`) with the same meaning: strictly inside a claim of that class. One word, one meaning: containment.
- `exists ~…/` asks whether a path under a known folder exists, with one metadata call and no content read. It is the corroboration "Telegram Desktop is installed for this user" needs, without matching the folder by name.
- A known folder is a cheap seed: its node's preorder range makes `in` one range check, and `at` hands the engine its candidate directly.

## The long form

**Status:** Unit 3 (`classify`, `suggest`, `lens`, `platforms`); tables Unit 4; actions Unit 6. **None of the examples in this section parse today.**

The long form splits what the short form fuses. `classify` makes claims; `suggest` speaks inside them; `lens` highlights part of the arbitrary tree; `table` reads manifests. A pack may mix short-form and long-form rules in one file.

### `classify`

```
classify [Windows directory]
class = windows-os/v1
description = the Windows directory: `System32` and `WinSxS` under the OS-resolved Windows folder
filter {
  $w at ~windows/
  $w has System32
  $w has WinSxS
  then $w/
}
```

| key | required | meaning |
|---|---|---|
| `class` | yes | `name/vN`, declared in `pack.toml` under `[classes."name/vN"]`, where `exclusive = true` makes the claim exclusive. A class with no `exclusive` line is open. |
| `description` | yes | why this is what the rule says it is; shown beside the claim in "Recognized". |
| `identity` | no | a template giving a stable identity, such as `steam:app:{$app.appid}`. |
| `confidence` | no, default 1.0 | the claim tie-break. |
| `platform` | no | narrows the pack's `platforms` for this rule. |

A classify rule produces a claim and nothing else. It never proposes removal.

### `suggest … within`

```
suggest [Windows Update downloads]
kind = cache/v1
disposition = review
within windows-os/v1
description = update packages Windows Update already installed; Windows removes them itself from Storage settings
action = windows.storage-settings
filter {
  claim $w is windows-os/v1
  then $w/SoftwareDistribution/Download/
}
```

- `within` names the class this suggestion speaks inside, and the filter binds it in one of two ways: `claim $g is <class>` binds `$g` to the claimed node itself, with every node field plus every fact of the claim, and the target is a path from it; `$f in <class>` binds `$f` to any node strictly inside such a claim, for suggestions that judge files one by one.
- The target must lie inside that claim, and the claim must belong to this pack. Suggestions arise only inside a classified subtree, and only from the pack that owns it.
- The keys are the short form's (`description`, `kind`, `confidence`, `disposition`) plus `action` (see [Actions](#actions)). Without `action`, the action is a permanent delete.
- A suggestion produces a verdict exactly as a short-form rule does: same provenance, same conflicts, same nesting.

### `lens`

```
lens [Old downloads]
description = in your Downloads folder, last modified {modified_age} days ago, {size} bytes; it may be your only copy
filter {
  $f in ~downloads
  $f.is_file
  $f.modified before 2mo ago
  then $f
}
```

A lens is the one rule that looks at the **arbitrary** tree, and it can only highlight. Its targets must be arbitrary nodes (no claim contains them); it has no `kind`, no `disposition`, no `action`, and it never puts anything on the delete list. The "Other files" view offers each lens as a filter, and `nomnom scan --view other --lens "<pack> [Title]"` prints the same list. The user picks from it by hand, as from any other part of the arbitrary tree. A claim takes its subtree out of every lens, so once a Telegram pack claims `~downloads/Telegram Desktop/`, those files are judged by Telegram's own suggestions and never appear under "Old downloads".

### `platforms`

`pack.toml` lists the platforms a pack applies to (`platforms = ["windows", "macos", "linux"]`), and a rule may narrow it with `platform =`. Packs and rules for another platform are dropped when the pack is compiled, so a skipped pack costs nothing at scan time. `pack lint` refuses a pack that names a known folder undefined on a platform it lists.

### Tables

**Status:** Unit 4.

A table is a bounded read of named manifest files in one of a closed set of formats — `vdf`, `json`, `toml`, `ini`, `plist` — parsed into rows of typed fields. It is how a pack learns what structure alone cannot say: which libraries Steam itself lists, which game a folder belongs to, when it was last played. It is also how `dist/` and `build/` come back: no rule claims them today, because a name is not evidence, and a later pack can restore them by reading the project's own config (a Vite or TypeScript config that names its output directory).

```
table apps = vdf
  file {
    $lib has steamapps
    then $lib/steamapps/appmanifest_*.acf
  }
  row AppState
  appid      num  = appid
  name       str  = name
  installdir name = installdir
```

- `table <name> = <format>` opens the table. `file { … then … }` is an ordinary filter whose target is the file to read; its segments must spell a literal stem or extension, and `pack lint` rejects a bare `*`.
- `row <path>` says where rows sit in the parsed document; `*` iterates the children of a key, and `$key` is that child's own key.
- Each field line is `<name> <type> = <key>`. Types: `num`, `str`, `name` (one validated file-name component: no separator, no `..`), `path` (compared with catalog paths only, never opened), `time` (seconds since the epoch), `minutes`.
- `merge by <field>: <reducer> <field>, …` collapses rows sharing a key, with the reducers `max`, `min`, `sum` and `any`. That is the whole relational algebra: one-hop equality joins and fixed reducers. The language will be pushed to grow into SQL; it does not.

In a filter, `row $r in <table> where $r.<field> == <expr>` requires a matching row and binds its fields; `row $r in <table> from $v` takes the rows read from files under `$v`'s own anchor. `with $r in <table> where …` is the optional join: when the table was read completely and no row matches, the claim gets the bool fact `<table>_found = false`; when one matches, `true`, and its fields; when the table is incomplete, `<table>_found` and every field are absent.

The fields of every bound row become the claim's **facts**, flattened by name. A name bound by two rows is a parse error, except when a `where` equates them.

**Manifest values never become paths to open.** A value can only be compared with a catalog path or name, select one validated child component (`then $lib/steamapps/common/{$app.installdir}/`, where a missing child means no claim), fill a typed handler argument, or be rendered into a description.

## Reads and caps

**Status:** Unit 4.

The language used to promise that no rule reads the inside of a file. Tables end that promise, and replace it with a narrower one: **every read is a file name spelled in a pack, inside a structure the pack already proved, parsed into typed fields.** There is no hashing, no byte comparison, no blob, and no path taken from a file's contents.

**What may be read.**

- The file is the target of a table's `file` filter, so its name is spelled in the pack.
- It lies inside an anchor that passed its structural checks — a **verified anchor**.
- It is a regular file and not a reparse point: checked with `symlink_metadata`, and opened with `FILE_FLAG_OPEN_REPARSE_POINT` on Windows.
- Its catalog size is under the per-file cap before it is opened, and the reader reads at most `cap + 1` bytes, so a file that grew since the scan counts as over the cap.

**Cross-volume reads are allowed.** A read may cross to another volume when its anchor is verified, and verification may itself cross volumes: the Steam client on `C:` reads `libraryfolders.vdf`, and a library folder on `D:` becomes a verified anchor only because the client's own manifest lists it. A folder on `D:` that merely holds `steamapps/` and is not listed is not a library, and Steam's play data in `C:`'s `localconfig.vdf` joins to games on any volume. This is about reading. A permanent delete stays fenced to the scan root and to its volume, as [Guards](#guards-belong-to-the-engine-not-to-rules) says. An `Action::Request` is not fenced the same way: it hands the removal to the owning program and deletes nothing itself, so a request onto a verified anchor — the `D:` library above — is allowed even though that anchor sits outside the scan root. See [Actions](#actions).

**Ceilings.** The engine's are hard: **16 MiB per file; 4,096 files or 64 MiB per pack; 256 MiB per run.** `pack.toml` may only lower them:

```toml
[reads]
max_files = 2048
max_bytes = "48mib"
```

Parsing is bounded too: nesting depth 64, at most 10⁶ parsed nodes. A cap hit or a parse error marks the table **incomplete**, so every fact derived from it is absent, and absent means keep. A game whose `localconfig.vdf` was over the cap is never called unplayed.

**When reads happen.** Reads run automatically after the scan, and only for packs whose anchors the scan recognized; a pack whose anchor is not on disk reads nothing. They are part of the analysis pipeline: Scan → … → Index (resolves known folders and table anchors) → **Inspect** (the reads) → **Classify** (claims, then the ownership and byte-split pass) → **Match** (suggestions, seeded from claim ranges) → Group.

**Every read is logged.** The read log records, per file, the pack, the table, the path, the bytes read and the outcome — parsed with N rows, too large, reparse point, unreadable, parse error, or cap reached. The CLI prints it with `nomnom suggest --reads` and in the `--json` field `reads`, and prints a one-line summary on stderr ("inspected 143 files (2.1 MiB) for 1 pack"). The GUI shows the same list in a "Files read" view. `exists` checks appear in the log as well.

## Actions

**Status:** Unit 6. Today every reclaimable verdict is a permanent delete.

```
action = steam.uninstall(appid = $g.appid)
action = windows.storage-settings
```

A suggestion's `action` names a **handler** from a registry compiled into the binary, with arguments drawn from the claim's facts.

- The registry holds fixed templates with typed parameters. `steam.uninstall(appid: num)` renders `steam://uninstall/<appid>`; `windows.storage-settings` opens Windows' own Storage settings page, which does the cleanup with its own elevation.
- **Arguments are typed — numbers or enums, never strings** — so nothing can be smuggled into a URI. A pack never defines a scheme or a template. The parser checks the handler name and its argument types against the registry, and the pack must list each handler it uses in `pack.toml`: `handlers = ["steam.uninstall"]`.
- A kind may declare `action = "required"` in `pack.toml`, so a rule of that kind cannot fall back to a delete. **Steam games are removed only through `steam://uninstall/<appid>`, never by deleting their folder**: Steam owns its library and confirms the removal itself.
- The plan stores `Action::Request { path, handler, args }`, with **no URI string**. The `path` is the claimed folder. Unlike a delete, it may lie outside the scan root and on another volume, when the claim's anchor is a **verified anchor** — a Steam library the client's own `libraryfolders.vdf` lists, for example (see [Reads and caps](#reads-and-caps)). A raw permanent delete keeps the existing guard: refused outside the scan root or on another volume, with no such exception, because a delete unlinks the bytes itself and a request only asks the owning program to. Validation re-renders the URI from the handler and its arguments, so a plan loaded from JSON cannot carry a URI.
- Before launching, apply checks that the scheme handler is registered on this machine. If it is not, that one record fails and the run continues.
- The apply log gains a `requested` status, counted separately: "deleted 3.1 GB · requested 48.2 GB from Steam (unconfirmed)". The next scan shows what the owning program actually did.

**Deletion elsewhere is permanent.** No recycle bin, no undo. So approval comes before anything happens: nothing is selected by default, a rule's suggestions are approved one rule at a time (opt-in), and per-path exclusions persist across runs. The CLI and the GUI offer exactly the same approvals, exclusions and actions.

**Out of scope for now:** tool-command handlers (`pnpm store prune`, `Dism /StartComponentCleanup`) and elevated apply. A command brings PATH hijacking, argument injection, elevation, exit codes, cancellation and long runs, and is a separate decision. Until then, things that need elevation are offered as an OS-tool request where Windows provides one, and the pnpm store stays a review delete with an honest sentence.

## Worked example: `builtin.steam`

**Status:** classify rules Unit 5, suggestions Unit 6, tables Unit 4. Does not parse today.

```toml
# pack.toml
name = "builtin.steam"
version = "1.0.0"
platforms = ["windows", "macos", "linux"]
handlers = ["steam.uninstall"]

[classes."steam-client/v1"]
exclusive = true
[classes."steam-library/v1"]
exclusive = true
[classes."steam-game/v1"]
exclusive = true

[kinds."unplayed-game/v1"]
disposition = "review"
confidence = 0.9
action = "required"
```

```
table libraries = vdf
  file {
    $s has steamapps
    $s has userdata
    then $s/steamapps/libraryfolders.vdf
  }
  row libraryfolders.*
  path path = path

table apps = vdf
  file {
    $lib has steamapps
    then $lib/steamapps/appmanifest_*.acf
  }
  row AppState
  appid      num  = appid
  name       str  = name
  installdir name = installdir

# VDF keys match case-insensitively. Several accounts on one PC: the most
# recent play wins, and play time adds up.
table played = vdf
  file {
    $s has steamapps
    $s has userdata
    then $s/userdata/*/config/localconfig.vdf
  }
  row UserLocalConfigStore.Software.Valve.Steam.apps.*
  appid       num     = $key
  last_played time    = LastPlayed
  playtime    minutes = Playtime
  merge by appid: max last_played, sum playtime

classify [Steam client]
class = steam-client/v1
description = Steam's install folder: `steamapps/`, `userdata/` and `{$exe}` side by side
filter {
  $s has steamapps
  $s has userdata
  $s has steam.exe | steam.sh as $exe
  then $s/
}

classify [Steam library]
class = steam-library/v1
description = a Steam library, listed in Steam's own `libraryfolders.vdf`
filter {
  $lib has steamapps
  row $l in libraries where $l.path == $lib.path
  then $lib/steamapps/
}

classify [Steam game]
class = steam-game/v1
identity = steam:app:{$app.appid}
description = {$app.name}, installed by Steam: `appmanifest_{$app.appid}.acf` names `{$app.installdir}`
filter {
  $lib has steamapps
  row $l in libraries where $l.path == $lib.path
  row $app in apps from $lib
  with $p in played where $p.appid == $app.appid
  then $lib/steamapps/common/{$app.installdir}/
}

suggest [Steam game not played in 180 days]
kind = unplayed-game/v1
within steam-game/v1
description = {$g.name}: {subtree_size} bytes, last played {$g.last_played} days ago by any Steam account on this PC; Steam can reinstall it from your library
action = steam.uninstall(appid = $g.appid)
filter {
  claim $g is steam-game/v1
  $g.last_played before 180d ago
  then $g
}

suggest [Steam game never played, installed long ago]
kind = unplayed-game/v1
within steam-game/v1
description = {$g.name}: {subtree_size} bytes, nothing in it changed for {max_descendant_age} days, and no Steam account on this PC has played it (no `LastPlayed` in any `localconfig.vdf`)
action = steam.uninstall(appid = $g.appid)
filter {
  claim $g is steam-game/v1
  not $g.played_found
  $g.max_descendant_age >= 180d
  then $g
}
```

What this shows:

- **Three nested exclusive claims from one pack.** The default library sits inside the client folder and games sit inside libraries; all are kept because they share a pack. No other pack's `cache` or `build-output` rule fires inside a game folder.
- **The library is verified by Steam's own manifest**, not by holding `steamapps/`. A copied or abandoned `steamapps/` on another drive is not a library, and its folders stay in "Other files".
- **The game folder comes from the manifest's `installdir`**, a validated single component. If the folder is missing, there is no claim.
- **The never-played suggestion is safe by construction.** `played_found` is false only when every `localconfig.vdf` was read and parsed in full and none lists the game. A cap hit, a parse error or a missing file makes it absent, and `not` of an absent bool is false.
- **The threshold is fixed at 180 days** in v1. A user who wants another one installs an override pack; how an override pack may speak inside `builtin.steam`'s claims is still open (see [Packs](#packs)).
- **Removal is a request.** The plan line reads `request  steam.uninstall 1245620  48.2 GB  Elden Ring …  builtin.steam [Steam game not played in 180 days]`, and Steam asks the user to confirm.
- **The library can sit on another volume.** A library on `D:` while the scan root is `C:` is ordinary: `libraryfolders.vdf` makes it a verified anchor, so the request is allowed to name a game folder there. Nothing is deleted outside the scan root or on another volume — the request only asks Steam to act, and Steam owns that volume's bytes already.

## Worked example: Downloads

**Status:** today's `builtin.downloads` is a short-form `stale-download/v1` review rule using `under Downloads/`, which also matches folders that are merely named Downloads. Unit 2 stops it suggesting; Unit 3 ports it to the lens below.

Downloads is arbitrary space: the user put those files there, and a stale download is routinely the only copy of something that cannot be downloaded again. So it is not a tool claim and gets no suggestions. It is an age-based **lens** over "Other files", where the user decides:

```
lens [Download not modified in 2 months]
description = in your Downloads folder, last modified {modified_age} days ago, {size} bytes; it may be your only copy
filter {
  $f in ~downloads
  $f.is_file
  $f.modified before 2mo ago
  then $f
}

lens [Download not opened in 2 months]
description = in your Downloads folder, last opened {accessed_age} days ago, {size} bytes; it may be your only copy
filter {
  $f in ~downloads
  $f.is_file
  $f.accessed before 2mo ago
  then $f
}
```

- `in ~downloads` resolves through the OS, so a Downloads folder the user moved is still found, and `Documents\Image-Line\Downloads` — FL Studio's sample downloads — never matches.
- Two lenses rather than one because the sentence must say which clock it read. Windows disables last-access updates by default, so `accessed` is usually absent there, and an absent time makes the comparison false rather than true.
- A program that writes into Downloads with evidence leaves the lens by claiming its folder: Telegram Desktop, below.

```
classify [Telegram Desktop downloads]
class = telegram-downloads/v1
description = `Telegram Desktop/` in Downloads, and Telegram Desktop is installed for this user (`tdata/` in app data)
filter {
  $d at ~downloads/Telegram Desktop/
  exists ~app-data/Telegram Desktop/tdata/
  then $d/
}

suggest [Telegram download older than 90 days]
kind = stale-download/v1
within telegram-downloads/v1
description = saved from a Telegram chat {modified_age} days ago, {size} bytes; it may be your only copy
filter {
  $f in telegram-downloads/v1
  $f.is_file
  $f.modified before 90d ago
  then $f
}
```

## How the engine runs a pack

A rule's shape tells the engine where it can match, so no rule is run against every node. This is the same trick a browser uses for CSS selectors: match from the right, starting at the most specific name.

1. **Key.** Each rule gets a key from its shape — the deepest literal name in its `then` path (`node_modules`), or failing that, the anchor's own name pinned by `$v.name`, `$v.dir.name` or `$v.file.name == "literal"`, or failing that, a literal `has` name (the anchor must hold that child), or failing that, its `under` name. One pass over the catalog collects the nodes carrying any key name, and a rule is tried only at those nodes. From Unit 3, two more seeds exist: a known-folder node (`at`, `in ~…`) and the ranges of claims of a class (`claim $g is`, `in class/vN`).
2. **Climb, then check.** From a keyed node the engine climbs back up the `then` path to the anchor and tests the constraints there, cheapest first: `under` (a binary search over the subtree ranges of every node with that name), then bool and numeric fields, then one pass over the anchor's children that answers every `has` and `lacks` at once, then string fields.
3. **Universal rules.** A rule with no literal name anywhere has no key and is checked at every node. That is allowed today, and it is timed on its own line under `NOMNOM_TIMINGS=1`, so a pack that makes a scan slow says which rules did it. From Unit 3, `pack lint` refuses a universal rule in a built-in pack.

Cost stays O(nodes) plus seed hits, however many packs are loaded.

## Conflicts

Several rules can target one node. Resolution is deterministic, in this order:

1. highest `confidence`
2. pack precedence (later-resolved pack wins)
3. rule order within the pack (earlier rule wins)

The trust cap (below) applies to the winner, after resolution. Then nesting is resolved: a target inside another target is dropped, whatever its confidence, because the outer verdict already decides it. Claims resolve by the same three steps (see [Ownership](#ownership)).

The winning verdict records which pack and rule produced it. With packs coming from the network, "why does nomnom want to delete this" must be answerable down to the rule, so provenance is part of the verdict rather than a debugging aid.

## Packs

```
mypack/
  pack.toml        name, version, kinds; from Unit 3 also platforms, classes, handlers, [reads]
  rules/*.nom
  fixtures/*.tree  Unit 3: golden tests, with a .expect beside each
```

```toml
name = "rust"
version = "0.2.0"

[kinds."toolchain-cache/v1"]
disposition = "reclaimable"
confidence = 0.7
```

Rule files load in file-name order, so rule order — the last conflict tie-break — is the same on every machine.

Resolution order, later overriding earlier:

1. built-in, compiled into the binary: one pack per tool that creates the files, named `builtin.<tool>`, in alphabetical order — `builtin.ableton`, `builtin.after-effects`, `builtin.bun`, `builtin.cargo`, `builtin.chrome`, `builtin.cmake`, `builtin.cocoapods`, `builtin.cpython`, `builtin.dart`, `builtin.dotnet`, `builtin.downloads`, `builtin.edge`, `builtin.firefox`, `builtin.go`, `builtin.gradle`, `builtin.maven`, `builtin.mypy`, `builtin.next`, `builtin.npm`, `builtin.nuget`, `builtin.pip`, `builtin.pnpm`, `builtin.pytest`, `builtin.ruff`, `builtin.tox`, `builtin.uv`, `builtin.venv`, `builtin.vscode`, `builtin.windows-update` and `builtin.yarn`. There is no catch-all pack: every rule matches only on its tool's evidence (see [Evidence](#evidence-every-rule-names-its-tool)), so the order among built-ins decides nothing
2. user — `%LOCALAPPDATA%\nomnom\packs\`
3. project — `./.nomnom/packs/`
4. `--pack <dir>`, explicit

Built-in packs live in `crates/nomnom-core/packs/<pack>/`. Today each one is listed by hand. From Unit 3, a `build.rs` enumerates `packs/` and embeds every pack it finds, and a test fails on any stray file under `packs/` that is not part of a pack, so hundreds of packs need no registration list to keep in sync.

Git-backed packs are fetched by URL and **pinned to a commit**, never to a branch, and recorded in `.nomnom/packs.lock` with a content checksum. A pack that changes under a fixed reference is a supply-chain event, so the lock is what is loaded and a drifting remote is an error rather than an upgrade.

The trust gate (see [Trust](#trust)) applies only to a pack fetched from outside this machine — the community pack repository and a git-pinned pack. The user-tier, project-tier and `--pack <dir>` packs above are written locally and load without it; built-ins are trusted by construction.

```
nomnom pack add github.com/ranolp/nomnom-packs/rust
nomnom pack add https://git.example/packs.git@a1b2c3d
```

Cached at `%LOCALAPPDATA%\nomnom\packs\<host>\<org>\<repo>@<sha>`.

Packs graduate into the built-in set by pull request, the way Mole keeps one catalog in its own tree.

**Open: override packs.** Suggestions come only from the pack that owns the claim, so a pack that wants to change `builtin.steam`'s 180-day threshold cannot simply add its own `suggest … within steam-game/v1`. The likely shape is an explicit `extends = ["builtin.steam"]` in `pack.toml`, shown at trust time, under which the extending pack may suggest inside the named pack's classes and wins conflicts by ordinary pack precedence. Not decided; do not implement it before it is.

## Trust

### Today: untrusted packs cannot delete

**Status:** implemented; replaced in Unit 7.

A rule from any pack other than the built-in ones is **capped at `disposition = review`** until the user runs `nomnom pack trust <name>`. A pack that declares `reclaimable` is downgraded, and the CLI says why.

The language is total, so the worst a malicious pack can do is misclassify — but misclassification is precisely the harm here, because the next step deletes files. The cap makes the failure mode "a human is shown a bad suggestion" instead of "a stranger's repository chose what to remove from your disk". Trust is granted per pack, deliberately, once.

### From Unit 7: an untrusted pack is not loaded

**Status:** Unit 7.

Once packs can read files and request actions, a review cap is no longer enough for a pack that came from outside: a pack that may claim a folder can hide it from every other pack, and a pack that may read can read. So **a pack fetched from outside this machine is not loaded at all until it is trusted.** That is the community pack repository and a git-pinned pack (see [Packs](#packs)); there is no "untrusted but loaded" tier for either. `nomnom pack trust <name>` is the gate to loading them.

A pack the user writes locally needs no such gate: the user-tier pack (`%LOCALAPPDATA%\nomnom\packs\`), the project-tier pack (`./.nomnom/packs/`), and a pack passed with `--pack <dir>` all load without trust, because the person who wrote the rule is the person about to run it. Built-in packs are trusted by construction.

- `pack add` and `pack trust` show what the pack will be able to do before the user agrees: the tables it reads (format and file pattern), the handlers it uses, the known folders it anchors to, the classes it claims exclusively.
- The lock records a digest of those capabilities beside the content checksum. An update that adds a capability needs `pack trust` again; an update that only changes rules within the trusted capabilities does not.
- A trusted pack holds exactly the capabilities it declared. Built-in packs hold every registry handler they declare.

## Testing a pack

**Status:** Unit 3.

### Golden fixtures

Every pack carries `fixtures/*.tree` files, and one test runs them all and compares the result with the `.expect` file beside each. A `.tree` file lists paths with a size and an age, plus inline manifest contents for tables:

```
# fixtures/unplayed.tree
Steam/steam.exe                                    3mib  10d
Steam/userdata/123/config/localconfig.vdf          <<<
"UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" {
  "1245620" { "LastPlayed" "1700000000" "Playtime" "620" }
} } } } }
>>>
Steam/steamapps/libraryfolders.vdf                 <<<
"libraryfolders" { "0" { "path" "{root}/Steam" } }
>>>
Steam/steamapps/appmanifest_1245620.acf            <<<
"AppState" { "appid" "1245620" "name" "ELDEN RING" "installdir" "ELDEN RING" }
>>>
Steam/steamapps/common/ELDEN RING/eldenring.exe    48gb  400d
```

The test builds the catalog in memory and reads tables through an in-memory reader, so no fixture touches the disk. The `.expect` file holds the claims, suggestions and lens hits as text; `NOMNOM_BLESS=1` rewrites it. The exact `.tree` syntax is fixed by Unit 3; the sketch above is its intent.

Each pack needs at least one fixture with a match and one look-alike that must not match — a `target/` without Cargo's signature, a `steamapps/` no manifest lists, a folder named Downloads that is not `~downloads`.

### `pack lint`

`nomnom pack lint <dir>` runs in CI over every built-in pack and refuses a pack unless:

- every rule has positive evidence beyond a bare name — `has`, `at`, `exists`, `row`, or a claim — or a name-only rule carries its official documentation URL in a comment;
- no rule is universal;
- `platforms` is declared, and every known folder it uses is defined on each of them;
- the pack has fixtures with a match and a look-alike;
- user-data kinds (`stale-download/v1` and any kind a pack declares over user files) are at most `review`;
- every handler is in the registry and listed in `pack.toml`;
- table `file` patterns spell a literal stem or extension.

## Guards belong to the engine, not to rules

A guard written into a rule protects the one path that rule is on. We read the incident history of Mole, a macOS cleaner with a far deeper deny-list than this one, and nearly every data-loss report there has the same shape: a guard that existed on the adjacent code path and was not wired into the one that fired. A `dist/` was deleted from inside a `node_modules` because the container check had no exclusion for it; a purge ran without the whitelist it was supposed to load; an existence probe failed open.

So a refusal is never a rule. It is applied to every candidate, after evaluation and before the verdict reaches a human:

- unknown means keep, for every fact, every probe and every read
- an open database and its `-wal`/`-shm`/`-journal` companions are never candidates
- reparse points are not descended through, not read, and their targets are not counted in a rolled-up size — NTFS junctions, OneDrive placeholders and pnpm's store links are all reparse points, and `is_symlink` does not cover any of them
- a permanent delete outside the scan root, or on another volume than the scan root, is refused; reads may cross volumes (see [Reads and caps](#reads-and-caps)), and so may an `Action::Request`, but only onto a verified anchor — a request hands the removal to the owning program and deletes nothing itself, so the delete's volume fence does not bind it (see [Actions](#actions))
- a run that could not verify something reports "I could not verify N items" rather than quietly including or excluding them

The other half of that history is worth stating too: every serious incident there was an unrecoverable one, because deletion was a permanent `unlink`. nomnom deletes permanently as well, with no recycle bin and no undo, so its safety has to come before the deletion rather than after it. Nothing is deleted that the user did not approve: every candidate starts unapproved, a rule or a path is approved one at a time, and an approval lasts one run. Exclusions persist per drive and can only shrink a plan. And the executor's own fences hold whatever the plan says: it refuses a drive root, a path outside the clean root, a path containing `..`, and the clean root itself.

## What the language does not do

### Facts the vocabulary still needs

Named here rather than in an issue tracker, because the vocabulary is a table and each of these is one row plus the Rust that computes it. Roughly in value order:

| fact | shape | what supplies it |
|---|---|---|
| `vcs_tracked` | field, bool | nearest ancestor holding `.git`, one cached `git ls-files` per repo root. Unresolvable means tracked, means keep. A committed `dist/` is indistinguishable from a generated one by name alone. |
| `contains *.pyc` | constraint | any descendant name matching the glob, not only a direct child as `has` checks. A `__pycache__` holding bytecode is build output; one holding anything else is somebody's oddly-named directory. This is the general shape of positive corroboration. |
| `is_reparse_point` | field, bool | `FILE_ATTRIBUTE_REPARSE_POINT` from the scan. Needed by the engine refusal above and the read check, not by rules. |
| `child_file_size("offline.bnk")` | field-like | size of a named child, expressing "this cache is big enough to be worth naming" without reading bytes. |
| `owner_installed("Slack")` | constraint | the host's installed-product set: uninstall registry keys, `%ProgramFiles%`, `WindowsApps`. Unresolvable means installed, means keep. `exists ~…/` covers the per-user case; this is the machine-wide one. Orphaned application data is worth `review` even with this, never `reclaimable`. |
| `in_use` | field, tri-state | Windows RestartManager. Unknown means in use, means keep. Until it exists, partial downloads and database files are engine refusals rather than rules. |
| `sibling_rank("app-*", version)` and `is_pinned` | needs a peer set | keep-newest-N across version-suffixed siblings, which is how every Squirrel/Electron application accumulates gigabytes. This is the first constraint that reads a set of peers rather than one node, so it changes the matcher's shape. `is_pinned` is inseparable from it, because updaters stage the next version before flipping the pointer at it. |

### Deliberately excluded

- **Computation.** No arithmetic, no string functions, no user-defined functions, no loops. Tables join one hop on equality and reduce with `max`, `min`, `sum`, `any`.
- **Arbitrary content reads.** A pack reads only files it names, in formats from the closed set, under the caps. Hashing and duplicate search are never automatic; they run only when the user asks.
- **New formats, known folders or handlers from a pack.** Each is a release of nomnom, reviewed once, shared by every pack.
- **Suggestions over the arbitrary tree.** A lens can highlight; only a claim can suggest.
