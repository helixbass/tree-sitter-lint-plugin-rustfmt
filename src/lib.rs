#![allow(clippy::into_iter_on_ref)]

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
    violation, Plugin, QueryMatchContext, Rule, RunKind,
};

pub type ProvidedTypes<'a> = ();

pub fn instantiate() -> Plugin {
    Plugin {
        name: "rustfmt".to_owned(),
        rules: vec![rustfmt_rule()],
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
    if context.file_run_context.run_kind == RunKind::NonfixingForSlice {
        return;
    }

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
            drop(stdin);
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
    assert_eq!(file_with_mismatches.name, "<stdin>");

    for mismatch in file_with_mismatches.mismatches {
        assert!(
            (mismatch.original.is_empty() || mismatch.original.ends_with('\n'))
                && (mismatch.expected.is_empty() || mismatch.expected.ends_with('\n')),
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
            RopeOrSlice::Slice(slice) => {
                let newline_offsets = get_newline_offsets(slice).collect::<Vec<_>>();
                Range {
                    start_byte: if mismatch.original_begin_line >= 2 {
                        newline_offsets
                            .get(mismatch.original_begin_line - 2)
                            .map_or(slice.len(), |&newline_offset| newline_offset + 1)
                    } else {
                        0
                    },
                    end_byte: newline_offsets
                        .get(mismatch.original_end_line - 1)
                        .map_or(slice.len(), |&newline_offset| newline_offset + 1),
                    start_point: Point {
                        row: mismatch.original_begin_line - 1,
                        column: 0,
                    },
                    end_point: Point {
                        row: mismatch.original_end_line,
                        column: 0,
                    },
                }
            }
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

#[derive(Debug, Deserialize)]
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

fn get_newline_offsets(slice: &[u8]) -> impl Iterator<Item = usize> + '_ {
    slice
        .into_iter()
        .copied()
        .enumerate()
        .filter_map(|(index, byte)| (byte == b'\n').then_some(index))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tree_sitter_lint::{rule_tests, RuleTester};

    #[test]
    fn test_basic() {
        RuleTester::run(
            rustfmt_rule(),
            rule_tests! {
                valid => [
                    "fn whee() {}\n",
                ],
                invalid => [
                    {
                        code => "fn whee( ) {}\n",
                        output => "fn whee() {}\n",
                        errors => 1,
                    }
                ]
            },
        );
    }
}
