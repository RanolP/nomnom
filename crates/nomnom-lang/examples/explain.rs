//! Print the diagnostic for a rule file, so error quality can be looked at
//! rather than taken on trust.
//!
//! `cargo run -p nomnom-lang --example explain -- path/to/rules.nom`
//! With no argument it parses a deliberately broken rule built in below.

use nomnom_lang::{Kinds, Source, parse};

const BROKEN: &str = "\
[Stale download]
description = in Downloads, last modified {modified_age} days ago
kind = stale-download/v1
filter {
  $f under Downloads/
  $f.size > \"big\"
  then $f
}
";

fn main() {
    let source = match std::env::args().nth(1) {
        Some(path) => {
            let text = std::fs::read_to_string(&path).expect("read rule file");
            Source::new(path, text)
        }
        None => Source::new("rules/downloads.nom", BROKEN),
    };
    match parse(&source, &Kinds::builtin()) {
        Ok(rules) => println!("ok: {} rule(s)", rules.len()),
        Err(diagnostic) => print!("{diagnostic}"),
    }
}
