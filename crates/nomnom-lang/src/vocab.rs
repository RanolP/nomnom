//! The one table of everything a rule may say.
//!
//! Fields are declared **once**, in the [`vocabulary!`] invocation at the
//! bottom of this file. Adding `$f.owner == "root"` is one line there and
//! nothing else: the macro derives the enum variant, the name lookup the parser
//! uses, the type the type checker uses, and the `&'static` table anyone (an
//! evaluator, a docs generator, a completion list) can iterate. The evaluator
//! matches on the generated enum, so a new entry shows up as a
//! non-exhaustive-match error there rather than as a silent no-op at runtime.
//!
//! Questions about a node's surroundings are not fields: `has`, `lacks` and
//! `under` are filter syntax, because an engine can index a name it can see in
//! the grammar and cannot index one hidden inside a function call.

use std::fmt;

/// The type system, entire. Five types, no variables, no inference.
///
/// `Size` and `Duration` are distinct from `Num` on purpose: `size > 100`
/// meaning 100 bytes is the exact class of silent wrongness a threshold
/// language cannot afford, so the unit is mandatory and checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    Str,
    Num,
    Size,
    Duration,
    Bool,
}

impl Ty {
    /// Whether `<`, `>`, `<=`, `>=` mean anything for this type.
    ///
    /// Ordering strings and booleans is expressible in the grammar but has no
    /// filesystem meaning, so it is rejected rather than given an arbitrary
    /// one. (`docs/lang.md` lists the operators without restricting them; this
    /// is the narrower reading.)
    pub fn is_ordered(self) -> bool {
        matches!(self, Ty::Num | Ty::Size | Ty::Duration)
    }

    /// What an author should type to produce a value of this type.
    pub fn example(self) -> &'static str {
        match self {
            Ty::Str => "a string such as `\"target\"`",
            Ty::Num => "a number such as `3`",
            Ty::Size => "a size such as `100kb` or `10mib`",
            Ty::Duration => "a duration such as `90d` or `12h`",
            Ty::Bool => "`true` or `false`",
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Ty::Str => "string",
            Ty::Num => "number",
            Ty::Size => "size",
            Ty::Duration => "duration",
            Ty::Bool => "bool",
        };
        f.write_str(name)
    }
}

/// One row of the field table.
#[derive(Debug, Clone, Copy)]
pub struct FieldDef {
    pub field: Field,
    pub name: &'static str,
    pub ty: Ty,
    pub doc: &'static str,
}

macro_rules! vocabulary {
    (
        fields { $($fvar:ident $fname:literal : $fty:ident , $fdoc:literal ;)* }
    ) => {
        /// A fact about the node under judgement.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Field { $($fvar),* }

        /// Every field, in declaration order.
        pub static FIELDS: &[FieldDef] = &[
            $(FieldDef { field: Field::$fvar, name: $fname, ty: Ty::$fty, doc: $fdoc }),*
        ];

        impl Field {
            pub fn def(self) -> &'static FieldDef {
                FIELDS.iter().find(|d| d.field == self).expect("every variant has a row")
            }
            pub fn name(self) -> &'static str { self.def().name }
            pub fn ty(self) -> Ty { self.def().ty }
            pub fn lookup(name: &str) -> Option<Field> {
                FIELDS.iter().find(|d| d.name == name).map(|d| d.field)
            }
        }
    };
}

vocabulary! {
    fields {
        Name        "name"         : Str,  "file-name component";
        DirName     "dir.name"     : Str,  "file-name component; matches only directories";
        FileName    "file.name"    : Str,  "file-name component; matches only files";
        Ext         "ext"          : Str,  "extension without the dot";
        Path        "path"         : Str,  "full path";
        Size        "size"         : Size, "own size in bytes";
        SubtreeSize "subtree_size" : Size, "rolled-up size, inclusive";
        FileCount   "file_count"   : Num,  "files in subtree";
        DirCount    "dir_count"    : Num,  "dirs in subtree";
        Depth       "depth"        : Num,  "distance from scan root";
        IsDir       "is_dir"       : Bool, "the node is a directory";
        IsFile      "is_file"      : Bool, "the node is a regular file";
        IsSymlink   "is_symlink"   : Bool, "the node is a symbolic link";
        ModifiedAge "modified_age" : Duration,
            "how long ago the node was modified; absent with no mtime";
        AccessedAge "accessed_age" : Duration,
            "how long ago the node was opened; absent with no atime";
        MaxDescendantAge "max_descendant_age" : Duration,
            "how long ago the most recently modified node in the subtree was modified";
        HasAccessed "has_accessed" : Bool, "the filesystem reported a last-access time";
    }
}

/// The closest vocabulary name to `given`, for a "did you mean" hint.
///
/// Edit distance, capped: a suggestion that is not obviously the intended word
/// is worse than no suggestion, because it sends the author down a wrong path.
pub fn nearest<'a>(given: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let budget = (given.len() / 3).max(1) + 1;
    candidates
        .map(|candidate| (edit_distance(given, candidate), candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut row = prev.clone();
    for (i, ca) in a.chars().enumerate() {
        row[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            row[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(row[j] + 1);
        }
        std::mem::swap(&mut prev, &mut row);
    }
    prev[b_chars.len()]
}
