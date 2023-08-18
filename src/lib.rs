use std::{
    io::Write,
    process::{Command, Stdio},
    sync::Arc,
};

use serde::Deserialize;
use tree_sitter_lint::{
    rule,
    tree_sitter::{Node, Point, Range},
    tree_sitter_grep::RopeOrSlice,
    violation, Plugin, QueryMatchContext, Rule,
};

pub fn instantiate() -> Plugin {
    Plugin {
        name: "rustfmt".to_owned(),
        rules: vec![rustfmt_rule()],
        event_emitter_factories: vec![],
    }
}

fn rustfmt_rule() -> Arc<dyn Rule> {
    rule! {
        name => "rustfmt",
        fixable => true,
        messages => [
            "unexpected_formatting" => "Unexpected formatting.",
        ],
        languages => [Rust],
        listeners => [
            "source_file:exit" => |node, context| {
                run_rustfmt(node, context);
            }
        ]
    }
}

// derived from https://github.com/oxidecomputer/rustfmt-wrapper/blob/main/src/lib.rs
fn run_rustfmt(node: Node, context: &QueryMatchContext) {
    let args = vec!["+nightly", "--unstable-features", "--emit", "json"];
    let mut command = Command::new("rustfmt")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = command.stdin.take().unwrap();
    match context.file_run_context.file_contents {
        RopeOrSlice::Slice(slice) => {
            stdin.write_all(slice).expect("Failed to write to stdin");
        }
        RopeOrSlice::Rope(rope) => {
            rope.write_to(stdin).expect("Failed to write to stdin");
        }
    }

    let output = match command.wait_with_output() {
        Ok(output) => output,
        Err(err) => {
            eprintln!("Running rustfmt failed: {err}");
            return;
        }
    };

    if !output.status.success() {
        let err_str = String::from_utf8(output.stderr).expect("Couldn't parse stderr as utf8");
        eprintln!("rustfmt returned an error: {err_str}");
        return;
    }

    let files_with_mismatches: Vec<FileWithMismatches> =
        serde_json::from_str(std::str::from_utf8(&output.stdout).expect("Didn't get JSON output"))
            .expect("Couldn't deserialize JSON output");
    if files_with_mismatches.is_empty() {
        return;
    }
    assert_eq!(files_with_mismatches.len(), 1);
    let file_with_mismatches = files_with_mismatches.into_iter().next().unwrap();
    assert_eq!(file_with_mismatches.name, "stdin");

    for mismatch in file_with_mismatches.mismatches {
        assert!(
            mismatch.original.ends_with('\n') && mismatch.expected.ends_with('\n'),
            "Looks like rustfmt is emitting entire lines?"
        );

        let range = match context.file_run_context.file_contents {
            RopeOrSlice::Rope(rope) => Range {
                start_byte: rope.line_to_byte(mismatch.original_begin_line - 1),
                end_byte: rope.line_to_byte(mismatch.original_end_line),
                start_point: Point {
                    row: mismatch.original_begin_line - 1,
                    column: 0,
                },
                end_point: Point {
                    row: mismatch.original_end_line,
                    column: 0,
                },
            },
            RopeOrSlice::Slice(_) => unimplemented!(),
        };
        context.report(violation! {
            node => node.descendant_for_byte_range(range.start_byte, range.end_byte).unwrap(),
            message_id => "unexpected_formatting",
            fix => |fixer| {
                fixer.replace_text_range(
                    range,
                    mismatch.expected.clone(),
                );
            }
        });
    }
}

#[derive(Deserialize)]
struct FileWithMismatches {
    name: String,
    mismatches: Vec<Mismatch>,
}

#[derive(Deserialize)]
struct Mismatch {
    original_begin_line: usize,
    original_end_line: usize,
    #[allow(dead_code)]
    expected_begin_line: usize,
    #[allow(dead_code)]
    expected_end_line: usize,
    original: String,
    expected: String,
}
