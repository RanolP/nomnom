//! Print the diagnostic for a rule file, so error quality can be looked at
//! rather than taken on trust.
//!
//! `cargo run -p nomnom-lang --example explain -- path/to/rules.nom`
//! With no argument it parses a deliberately broken rule built in below.

use nomnom_lang::{Source, parse};

const BROKEN: &str = r#"rule "stale-downloads" {
  when  ancestor("Downloads")
        and size > "big"
  then  label       = stale-download
        disposition = reclaimable
        confidence  = 0.7
}
"#;

fn main() {
    let source = match std::env::args().nth(1) {
        Some(path) => {
            let text = std::fs::read_to_string(&path).expect("read rule file");
            Source::new(path, text)
        }
        None => Source::new("rules/downloads.nom", BROKEN),
    };
    match parse(&source) {
        Ok(rules) => println!("ok: {} rule(s)", rules.len()),
        Err(diagnostic) => print!("{diagnostic}"),
    }
}
