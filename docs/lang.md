# nomnom rule language

A small, total language for saying what a file or directory **is**. It is not a scripting language and never becomes one: a rule can classify a path and nothing else. That restriction is what makes it safe to load rules written by someone else.

## Why a language at all

The judgement layer is a trait, `Judge`. Milestone 1 answered it with hand-written Rust. That works exactly until someone who is not us wants to teach nomnom about a toolchain we have never heard of — and then it fails completely, because the only way to add a rule is to recompile the tool.

So the rules move out of Rust and into data, and the data becomes distributable. Everything below follows from that one goal.

## A rule

```
rule "cargo-target" {
  when  dir.name == "target"
        and sibling("Cargo.toml")
  then  label       = build-output
        disposition = reclaimable
        unit        = true
        confidence  = 0.95
        reason      = "regenerable: Cargo build output, rebuilt by `cargo build` — `Cargo.toml` sits beside it"
}
```

`when` is a predicate over one node. `then` is what to conclude when it holds.

### `then` fields

| field | required | meaning |
|---|---|---|
| `label` | yes | what the path is. An identifier; packs may introduce their own. |
| `disposition` | yes | `keep`, `reclaimable`, or `review` |
| `reason` | yes | the sentence a human reads before approving. See below. |
| `confidence` | yes | `0.0`–`1.0` |
| `unit` | no, default `false` | this verdict speaks for the whole subtree |

`reason` is required and must be non-empty. It is the single load-bearing field in the product: it is what a human approves a deletion on, and from milestone 3 it is what a model writes. A rule whose reason restates its label (`"this is build output"`) is a broken rule — the reason names the evidence (`` "`Cargo.toml` sits beside it" ``).

A reason is a template. `{field}` interpolates any vocabulary field, and `{{` and `}}` are literal braces; nothing else is substitutable, so a reason cannot compute and cannot smuggle in a second expression language. A `size` renders as a bare byte count and a duration as a bare whole-day count, leaving the rule to supply the word:

```
reason = "cache directory `cache`: {subtree_size} bytes across {file_count} files, refilled on next use"
```

An unknown name inside braces is a parse error, not an empty substitution — a reason with a hole in it is shown to a human about to delete something.

`unit = true` means the directory is **one** decision, not one per file inside it. `node_modules` is the motivating case: 40,000 files, one verdict. Evaluation stops descending at a unit node, and the reclaimable total counts its `subtree_size` once. Setting this wrongly is the one authoring mistake that corrupts a total rather than merely adding a bad row, so it is explicit and validated, never inferred.

## Vocabulary

Purely structural. No rule reads the inside of a file, which keeps evaluation as cheap as the scan and means a pack cannot exfiltrate file contents.

**Fields**

```
name          file-name component
dir.name      same, but the rule only matches directories
file.name     same, but only files
ext           extension without the dot
path          full path
size          own size in bytes
subtree_size  rolled-up size, inclusive
file_count    files in subtree
dir_count     dirs in subtree
depth         distance from scan root
is_dir  is_file  is_symlink
is_duplicate  participates in a duplicate group (computed Rust-side)

modified_age        how long ago this node was modified; absent with no mtime
accessed_age        how long ago it was opened; absent with no atime
has_accessed        whether the filesystem reported an atime at all
max_descendant_age  how long ago the newest file anywhere in the subtree was modified
```

`max_descendant_age` exists because a directory's own mtime reflects only its entry list. `node_modules` whose contents are compiled against daily still has a months-old mtime the moment nothing is added or removed from its top level, so `modified_before(90d)` on a directory measures almost nothing. The rolled-up figure is what "nobody is using this project" actually means, and it costs nothing — it rolls up in the same bottom-up pass as `subtree_size`.

**Predicates**

```
sibling("Cargo.toml")     the parent has a child by this name
sibling_matches("*.sln")  the parent has a child whose name matches this glob
child("pyvenv.cfg")       this directory has a child by this name
ancestor("Downloads")     some ancestor is named this
matches("*.log")          glob against the name
modified_before(90d)
accessed_before(1y)       false when the platform reports no atime
```

Name comparison follows the platform: case-insensitive on Windows, case-sensitive elsewhere. This mirrors what the filesystem itself does — `Node_Modules` is the same directory on NTFS and a different one on ext4.

**Literals**

```
"string"                 true  false
0.95                     42
100kb  10mib  2gb        sizes, binary and decimal both understood
30d  6mo  1y  12h        durations
```

**Operators** — `and` `or` `not`, parentheses, and `== != < > <= >=`. That is the whole grammar. There is no assignment, no loop, no function definition, no recursion: every rule terminates, and its cost is bounded before it runs.

### Absent facts

Some facts a node simply cannot supply: `dir.name` on a file, `accessed_age` where Windows has last-access updates turned off, `max_descendant_age` where the walk was truncated by a permission error. The fact is **absent**, and every comparison against an absent fact is false.

That makes `not` the way to ask about absence, and it makes the safe direction the default one. `accessed_before(1y)` is false when the atime is unknown, so a rule that deletes on staleness stays silent rather than firing on a file it knows nothing about. The general invariant, which the engine holds and rules cannot opt out of: **unknown means keep.** Every fail-open probe in the tool we studied this design against became a data-loss incident.

## Conflicts

Several rules can match one node. Resolution is deterministic, in this order:

1. highest `confidence`
2. pack precedence (later-resolved pack wins)
3. rule order within the pack

The winning verdict records which pack and rule produced it. With packs coming from the network, "why does nomnom want to delete this" must be answerable down to the rule, so provenance is part of the verdict rather than a debugging aid.

## Packs

```
mypack/
  pack.toml       name, version, the labels it introduces
  rules/*.nom
```

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

Catalog-wide analysis stays in Rust. Duplicate detection needs a whole-catalog size-then-hash pass, so the language gets `is_duplicate` as a fact rather than the means to express the algorithm. The same will hold for anything else requiring a global view: the language sees one node at a time, and Rust supplies the facts that a single node cannot know about itself.

### Facts the vocabulary still needs

Named here rather than in an issue tracker, because the vocabulary is a table and each of these is one row plus the Rust that computes it. Roughly in value order:

| fact | shape | what supplies it |
|---|---|---|
| `vcs_tracked` | field, bool | nearest ancestor holding `.git`, one cached `git ls-files` per repo root. Unresolvable means tracked, means keep. The generic names — `build`, `dist`, `obj`, `out` — have no other defence, and a committed `dist/` is indistinguishable from a generated one by name alone. |
| `subtree_contains("*.pyc")` | predicate | any descendant name matching the glob. A `__pycache__` holding bytecode is build output; one holding anything else is somebody's oddly-named directory. This is the general shape of positive corroboration. |
| `child_matches("*.csproj")` | predicate | the `child` counterpart of `sibling_matches`. |
| `is_reparse_point` | field, bool | `FILE_ATTRIBUTE_REPARSE_POINT` from the scan. Needed by the engine refusal above, not by rules. |
| `child_file_size("offline.bnk")` | field-like | size of a named child, expressing "this cache is big enough to be worth naming" without reading bytes. |
| `owner_installed("Slack")` | predicate | the host's installed-product set: uninstall registry keys, `%ProgramFiles%`, `WindowsApps`. Unresolvable means installed, means keep. Orphaned application data is worth `review` even with this, never `reclaimable`. |
| `in_use` | field, tri-state | Windows RestartManager. Unknown means in use, means keep. Until it exists, partial downloads and database files are engine refusals rather than rules. |
| `sibling_rank("app-*", version)` and `is_pinned` | needs a peer set | keep-newest-N across version-suffixed siblings, which is how every Squirrel/Electron application accumulates gigabytes. This is the first predicate that reads a set of peers rather than one node, so it changes the matcher's shape; worth knowing before it is needed. `is_pinned` is inseparable from it, because updaters stage the next version before flipping the pointer at it. |

Three **rule shapes** are missing as well, and no predicate substitutes for them: match `X` but target a descendant of it (`.next/cache/*`, not `.next`); match `X` and also target a sibling (`.dart_tool` implies the `build/` beside it); match `X` but spare a named set of children (`.cargo/registry/cache` but not `src`, `index`, `git`). Without them such a rule must either delete too much or match nothing.

Deliberately excluded: parsing manifests — plists, JSON, version files. That is real content reading, it is format-specific, and everything it would buy is reachable through `sibling_rank` and `owner_installed`.
