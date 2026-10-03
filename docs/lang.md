# nomnom rule language

A small, total language for saying what a file or directory **is**. It is not a scripting language and never becomes one: a rule can classify a path and nothing else. That restriction is what makes it safe to load rules written by someone else.

## Why a language at all

Milestone 1 answered "what is this path" with hand-written Rust. That works exactly until someone who is not us wants to teach nomnom about a toolchain we have never heard of — and then it fails completely, because the only way to add a rule is to recompile the tool.

So the rules move out of Rust and into data, and the data becomes distributable. The language has two jobs at once: writing a rule should be easy, and the engine should be able to see enough of a rule's shape to run every rule in a pack without visiting every node once per rule. Everything below follows from those two goals.

## A rule

```
[build/ beside a manifest]
description = regenerable: build output, rebuilt by the project's build command — `{$marker}` sits beside it
kind = build-output/v1
filter {
  $dir has package.json | pyproject.toml | CMakeLists.txt as $marker
  then $dir/build/
}

[Stale download by access]
description = in Downloads, last opened {accessed_age} days ago, {size} bytes; may still be the only copy
kind = stale-download/v1
filter {
  $f.is_file
  $f under Downloads/
  $f.has_accessed
  $f.accessed_age >= 90d
  then $f
}
```

A rule opens with a `[Title]` line. The title is the rule's name: the CLI and the GUI show it beside every verdict the rule produces, as `pack [Title]`, and a pack name plus a title is the stable identity a verdict carries. Keys follow as `key = value`, one per line, and the rule ends with exactly one `filter { ... }` block. `#` starts a comment line.

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
- `{$name}` interpolates a name the filter bound: the filter's own variable renders the name of the node it matched, and a capture (`has … as $marker`) renders the child that satisfied it, in its on-disk spelling.

`{{` and `}}` are literal braces. Nothing else is substitutable, so a description cannot compute and cannot smuggle in a second expression language. An unknown field or an unbound `$name` inside braces is a parse error, not an empty substitution — a description with a hole in it is shown to a human about to delete something.

## Filters

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

A name in `has`, `lacks` or a `then` path is one file-name component. It is compared literally when it is a plain name and as a glob when it holds `*`, `?`, `[` or `{` — `has *.csproj | *.sln as $marker`. When several children satisfy a capturing `has`, the capture reports one that matches the first alternative written, and among those the smallest name, so a description is the same on every run.

`then $v` targets the variable's own node; `then $v/target/` targets its child `target`. A trailing `/` means the target must be a directory. The scan root is never a target: it is the fence the whole plan sits inside.

### Each target is one decision

A target is **one** decision for its whole subtree, not one per file inside it. `node_modules` is the motivating case: 40,000 files, one verdict, and the reclaimable total counts its rolled-up size once. Nothing inside a target is judged again by a rule: when one rule's target sits inside another's, the outer target swallows the inner one.

### Shapes this makes reachable

Because the target is a path from the matched node rather than the matched node itself, three shapes that the first version of this language could not express are now ordinary rules:

- **Match `X`, target a descendant of it.** `$p has .next / then $p/.next/cache/` takes `.next/cache` and leaves the rest of `.next` alone.
- **Match `X`, target a sibling.** `$p has .dart_tool / then $p/build/` takes the `build/` that a `.dart_tool` beside it vouches for.
- **Match `X`, spare named children.** `then $p/.cargo/registry/cache/` takes the registry's download cache and never touches `src`, `index` or `git`, which are its siblings.

## Vocabulary

Purely structural. No rule reads the inside of a file, which keeps evaluation as cheap as the scan and means a pack cannot exfiltrate file contents.

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
```

`max_descendant_age` exists because a directory's own mtime reflects only its entry list. `node_modules` whose contents are compiled against daily still has a months-old mtime the moment nothing is added or removed from its top level, so `$d.modified_age >= 90d` on a directory measures almost nothing. The rolled-up figure is what "nobody is using this project" actually means, and it costs nothing — it rolls up in the same bottom-up pass as `subtree_size`.

Name comparison follows the platform: case-insensitive on Windows, case-sensitive elsewhere. This mirrors what the filesystem itself does — `Node_Modules` is the same directory on NTFS and a different one on ext4.

**Literals**

```
"string"                 true  false
0.95                     42
100kb  10mib  2gb        sizes, binary and decimal both understood
30d  6mo  1y  12h        durations
```

A comparison is type-checked when the rule is parsed: `$f.size >= 90d` is an error, and so is ordering a string or a bool. There is no assignment, no loop, no function definition, no recursion: every rule terminates, and its cost is bounded before it runs.

### Absent facts

Some facts a node simply cannot supply: `accessed_age` where Windows has last-access updates turned off, `max_descendant_age` where the walk was truncated by a permission error. The fact is **absent**, and every comparison against an absent fact is false.

That makes `not $f.has_accessed` the way to ask about absence, and it makes the safe direction the default one. `$f.accessed_age >= 1y` is false when the atime is unknown, so a rule that deletes on staleness stays silent rather than firing on a file it knows nothing about. The general invariant, which the engine holds and rules cannot opt out of: **unknown means keep.** Every fail-open probe in the tool we studied this design against became a data-loss incident.

## How the engine runs a pack

A rule's shape tells the engine where it can match, so no rule is run against every node. This is the same trick a browser uses for CSS selectors: match from the right, starting at the most specific name.

1. **Key.** Each rule gets a key from its shape — the deepest literal name in its `then` path (`node_modules`), or failing that, a literal `has` name (the anchor must hold that child), or failing that, its `under` name. One pass over the catalog collects the nodes carrying any key name, and a rule is tried only at those nodes.
2. **Climb, then check.** From a keyed node the engine climbs back up the `then` path to the anchor and tests the constraints there, cheapest first: `under` (a binary search over the subtree ranges of every node with that name), then bool and numeric fields, then one pass over the anchor's children that answers every `has` and `lacks` at once, then string fields.
3. **Universal rules.** A rule with no literal name anywhere has no key and is checked at every node. That is allowed, and it is timed on its own line under `NOMNOM_TIMINGS=1`, so a pack that makes a scan slow says which rules did it.

## Conflicts

Several rules can target one node. Resolution is deterministic, in this order:

1. highest `confidence`
2. pack precedence (later-resolved pack wins)
3. rule order within the pack (earlier rule wins)

The trust cap (below) applies to the winner, after resolution. Then nesting is resolved: a target inside another target is dropped, whatever its confidence, because the outer verdict already decides it.

The winning verdict records which pack and rule produced it. With packs coming from the network, "why does nomnom want to delete this" must be answerable down to the rule, so provenance is part of the verdict rather than a debugging aid.

## Packs

```
mypack/
  pack.toml       name, version, the kinds it declares
  rules/*.nom
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

1. built-in, compiled into the binary
2. user — `%LOCALAPPDATA%\nomnom\packs\`
3. project — `./.nomnom/packs/`
4. `--pack <dir>`, explicit

Git-backed packs are fetched by URL and **pinned to a commit**, never to a branch, and recorded in `.nomnom/packs.lock` with a content checksum. A pack that changes under a fixed reference is a supply-chain event, so the lock is what is loaded and a drifting remote is an error rather than an upgrade.

```
nomnom pack add github.com/ranolp/nomnom-packs/rust
nomnom pack add https://git.example/packs.git@a1b2c3d
```

Cached at `%LOCALAPPDATA%\nomnom\packs\<host>\<org>\<repo>@<sha>`.

### Untrusted packs cannot delete

A rule from any pack other than the built-in one is **capped at `disposition = review`** until the user runs `nomnom pack trust <name>`. A pack that declares `reclaimable` is downgraded, and the CLI says why.

The language is total, so the worst a malicious pack can do is misclassify — but misclassification is precisely the harm here, because the next step deletes files. The cap makes the failure mode "a human is shown a bad suggestion" instead of "a stranger's repository chose what to remove from your disk". Trust is granted per pack, deliberately, once.

## Guards belong to the engine, not to rules

A guard written into a rule protects the one path that rule is on. We read the incident history of Mole, a macOS cleaner with a far deeper deny-list than this one, and nearly every data-loss report there has the same shape: a guard that existed on the adjacent code path and was not wired into the one that fired. A `dist/` was deleted from inside a `node_modules` because the container check had no exclusion for it; a purge ran without the whitelist it was supposed to load; an existence probe failed open.

So a refusal is never a rule. It is applied to every candidate, after evaluation and before the verdict reaches a human:

- unknown means keep, for every fact and every probe
- an open database and its `-wal`/`-shm`/`-journal` companions are never candidates
- reparse points are not descended through and their targets are not counted in a rolled-up size — NTFS junctions, OneDrive placeholders and pnpm's store links are all reparse points, and `is_symlink` does not cover any of them
- a candidate outside the scan root, or on another volume, is refused
- a run that could not verify something reports "I could not verify N items" rather than quietly including or excluding them

The other half of that history is worth stating too: every serious incident there was an unrecoverable one, because deletion was a permanent `unlink`. nomnom deletes reversibly, through the platform's own recycle bin, with a journal that restores byte-for-byte. That is the one place the comparison deliberately does not apply.

## What the language does not do

### Facts the vocabulary still needs

Named here rather than in an issue tracker, because the vocabulary is a table and each of these is one row plus the Rust that computes it. Roughly in value order:

| fact | shape | what supplies it |
|---|---|---|
| `vcs_tracked` | field, bool | nearest ancestor holding `.git`, one cached `git ls-files` per repo root. Unresolvable means tracked, means keep. The generic names — `build`, `dist`, `obj`, `out` — have no other defence, and a committed `dist/` is indistinguishable from a generated one by name alone. |
| `contains *.pyc` | constraint | any descendant name matching the glob, not only a direct child as `has` checks. A `__pycache__` holding bytecode is build output; one holding anything else is somebody's oddly-named directory. This is the general shape of positive corroboration. |
| `is_reparse_point` | field, bool | `FILE_ATTRIBUTE_REPARSE_POINT` from the scan. Needed by the engine refusal above, not by rules. |
| `child_file_size("offline.bnk")` | field-like | size of a named child, expressing "this cache is big enough to be worth naming" without reading bytes. |
| `owner_installed("Slack")` | constraint | the host's installed-product set: uninstall registry keys, `%ProgramFiles%`, `WindowsApps`. Unresolvable means installed, means keep. Orphaned application data is worth `review` even with this, never `reclaimable`. |
| `in_use` | field, tri-state | Windows RestartManager. Unknown means in use, means keep. Until it exists, partial downloads and database files are engine refusals rather than rules. |
| `sibling_rank("app-*", version)` and `is_pinned` | needs a peer set | keep-newest-N across version-suffixed siblings, which is how every Squirrel/Electron application accumulates gigabytes. This is the first constraint that reads a set of peers rather than one node, so it changes the matcher's shape; worth knowing before it is needed. `is_pinned` is inseparable from it, because updaters stage the next version before flipping the pointer at it. |

The three **rule shapes** this section used to list as missing — target a descendant, target a sibling, spare named children — are solved by `then` paths; see [Shapes this makes reachable](#shapes-this-makes-reachable).

Deliberately excluded: parsing manifests — plists, JSON, version files. That is real content reading, it is format-specific, and everything it would buy is reachable through `sibling_rank` and `owner_installed`.
