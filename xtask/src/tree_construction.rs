//! The html5lib tree-construction suite, as maintained in WPT under
//! `html/syntax/parsing/resources/*.dat`.
//!
//! Each `.dat` file holds tests of the form
//!
//! ```text
//! #data
//! <p>One<p>Two
//! #errors
//! (1,3): expected-doctype-but-got-start-tag
//! #document
//! | <html>
//! |   <head>
//! |   <body>
//! |     <p>
//! |       "One"
//! ```
//!
//! optionally with `#document-fragment <context>`, `#script-on` / `#script-off`
//! and `#new-errors` sections. Error messages are not compared (html5ever's
//! wording differs); the tree dump is.

use std::collections::BTreeSet;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use catpaw_dom::{HtmlParseOptions, html5lib_dump, parse_html, parse_html_fragment};

use crate::wpt;

#[derive(clap::Args)]
pub struct Args {
    /// Only run `.dat` files whose name contains this substring.
    #[arg(long)]
    pub filter: Option<String>,
    /// Print the diff for every failure, including expected ones.
    #[arg(long)]
    pub verbose: bool,
    /// Rewrite tests/tree-construction-expectations.txt with the current failures.
    #[arg(long)]
    pub update_expectations: bool,
}

#[derive(Debug, Default)]
struct TestCase {
    data: String,
    fragment: Option<String>,
    script: Option<bool>,
    expected: String,
}

/// Parses a `.dat` file. Mirrors html5lib's `TestData` reader: a line that
/// starts with `#` opens a section; a section's text is its raw lines with one
/// trailing newline removed; the section that precedes a new `#data` loses one
/// more newline (the blank separator line).
fn parse_dat(text: &str) -> Vec<TestCase> {
    let mut cases = Vec::new();
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        let heading = line.trim_end_matches(['\n', '\r']);
        if let Some(name) = heading.strip_prefix('#') {
            if name == "data" && !sections.is_empty() {
                if let Some(last) = sections.last_mut()
                    && last.1.ends_with('\n')
                {
                    last.1.pop();
                }
                cases.push(build_case(&sections));
                sections.clear();
            }
            sections.push((name.to_string(), String::new()));
        } else if let Some(last) = sections.last_mut() {
            last.1.push_str(line);
        }
    }
    if !sections.is_empty() {
        cases.push(build_case(&sections));
    }
    cases
}

fn build_case(sections: &[(String, String)]) -> TestCase {
    let get = |name: &str| {
        sections.iter().find(|(k, _)| k == name).map(|(_, v)| {
            let mut v = v.clone();
            if v.ends_with('\n') {
                v.pop();
            }
            v
        })
    };
    let has = |name: &str| sections.iter().any(|(k, _)| k == name);
    TestCase {
        data: get("data").unwrap_or_default(),
        fragment: get("document-fragment").map(|s| s.trim().to_string()),
        script: if has("script-on") {
            Some(true)
        } else if has("script-off") {
            Some(false)
        } else {
            None
        },
        expected: get("document").unwrap_or_default(),
    }
}

fn run_case(case: &TestCase) -> std::result::Result<(), String> {
    let options = HtmlParseOptions {
        scripting_enabled: case.script.unwrap_or(true),
        ..HtmlParseOptions::default()
    };
    let actual = match &case.fragment {
        None => {
            let r = parse_html(&case.data, &options);
            html5lib_dump(&r.dom, r.dom.document())
        }
        Some(context) => {
            let r = parse_html_fragment(&case.data, context, &options);
            // html5ever parses fragments under a synthetic <html> root.
            let root = r
                .dom
                .first_child(r.dom.document())
                .expect("fragment parse produced no root element");
            html5lib_dump(&r.dom, root)
        }
    };
    let actual = actual.trim_end_matches('\n');
    if actual == case.expected {
        Ok(())
    } else {
        Err(format!(
            "--- input\n{}\n--- expected\n{}\n--- actual\n{}\n",
            case.data, case.expected, actual
        ))
    }
}

fn load_expectations(path: &Path) -> Result<BTreeSet<String>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

pub fn run(args: Args) -> Result<()> {
    let root = wpt::workspace_root();
    let wpt_dir = wpt::ensure_checkout(&root, &["html/syntax/parsing"])?;
    let resources = wpt_dir
        .join("html")
        .join("syntax")
        .join("parsing")
        .join("resources");
    let mut files: Vec<PathBuf> = fs::read_dir(&resources)
        .with_context(|| format!("listing {}", resources.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "dat"))
        .collect();
    files.sort();

    let expectations_path = root
        .join("tests")
        .join("tree-construction-expectations.txt");
    let expected_failures = load_expectations(&expectations_path)?;

    let mut total = 0usize;
    let mut failures: BTreeSet<String> = BTreeSet::new();
    let mut diffs: Vec<(String, String)> = Vec::new();

    for file in &files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        if let Some(f) = &args.filter
            && !name.contains(f.as_str())
        {
            continue;
        }
        let bytes = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        let text = String::from_utf8_lossy(&bytes);
        for (i, case) in parse_dat(&text).iter().enumerate() {
            total += 1;
            let id = format!("{name}:{i}");
            let outcome = catch_unwind(AssertUnwindSafe(|| run_case(case)))
                .unwrap_or_else(|_| Err("parser panicked".to_string()));
            if let Err(diff) = outcome {
                failures.insert(id.clone());
                diffs.push((id, diff));
            }
        }
    }

    let unexpected_failures: BTreeSet<String> =
        failures.difference(&expected_failures).cloned().collect();
    let unexpected_passes: BTreeSet<String> = if args.filter.is_none() {
        expected_failures.difference(&failures).cloned().collect()
    } else {
        BTreeSet::new()
    };

    for (id, diff) in &diffs {
        if args.verbose || unexpected_failures.contains(id) {
            println!("FAIL {id}\n{diff}");
        }
    }
    println!(
        "tree-construction: {total} tests, {} passed, {} failed ({} expected), {} unexpected failures, {} unexpected passes",
        total - failures.len(),
        failures.len(),
        failures.len() - unexpected_failures.len(),
        unexpected_failures.len(),
        unexpected_passes.len()
    );

    if args.update_expectations {
        let mut out = String::from(
            "# Known tree-construction failures, one `<file>.dat:<index>` per line.\n\
             # Regenerate with `cargo xtask tree-construction --update-expectations`.\n\
             #\n\
             # Categories (as of html5ever 0.40.1):\n\
             # - processing-instructions.dat, tests1.dat:39/43/46, html5test-com.dat:11:\n\
             #   WHATWG now parses `<?...>` as ProcessingInstruction nodes; html5ever\n\
             #   still emits bogus comments.\n\
             # - scripted_*.dat: need document.write during parsing (M1).\n\
             # - search-element.dat: html5ever does not treat <search> as special.\n\
             # - template.dat, tests_innerHTML_1.dat: html5ever tree-builder gaps\n\
             #   (frameset-ok after <template>, form pointer in templates, <input> in select).\n\
             # - webkit02.dat:44-47: html5ever only runs the <selectedcontent> cloning\n\
             #   hook on an explicit </option>; these options close implicitly.\n",
        );
        for id in &failures {
            out.push_str(id);
            out.push('\n');
        }
        fs::write(&expectations_path, out)
            .with_context(|| format!("writing {}", expectations_path.display()))?;
        println!("wrote {}", expectations_path.display());
        return Ok(());
    }

    for id in &unexpected_passes {
        println!("UNEXPECTED PASS {id}: remove it from tests/tree-construction-expectations.txt");
    }
    if !unexpected_failures.is_empty() || !unexpected_passes.is_empty() {
        bail!("tree-construction results do not match tests/tree-construction-expectations.txt");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dat_sections_like_html5lib() {
        let text = "#data\nTest\n#errors\n(1,0): x\n#document\n| <html>\n|   \"a\n\nb\"\n\n#data\n<p>\n\n#document-fragment\nsvg path\n#script-off\n#document\n| <p>\n";
        let cases = parse_dat(text);
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].data, "Test");
        assert_eq!(cases[0].expected, "| <html>\n|   \"a\n\nb\"");
        assert_eq!(cases[0].fragment, None);
        assert_eq!(cases[1].data, "<p>\n");
        assert_eq!(cases[1].fragment.as_deref(), Some("svg path"));
        assert_eq!(cases[1].script, Some(false));
        assert_eq!(cases[1].expected, "| <p>");
    }
}
