#![allow(clippy::into_iter_on_ref)]

use std::{
    io::Write,
    ops,
    process::{Command, Stdio},
    sync::Arc,
};

use itertools::Itertools;
use serde::{Deserialize, Serialize};
use tracing::trace;
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
    if matches!(
        context.file_run_context.run_kind,
        RunKind::NonfixingForSlice
    ) {
        return;
    }

    let line_ranges = match context.file_run_context.run_kind {
        RunKind::FixingForSliceInitial { context }
            if context.edits_since_last_fixing_run.is_some()
                && context.last_fixing_run_violations.is_some() =>
        {
            let edits_since_last_fixing_run = context.edits_since_last_fixing_run.as_ref().unwrap();
            Some(
                edits_since_last_fixing_run
                    .get_new_ranges()
                    .into_iter()
                    .map(|range| range.start_point.row..range.end_point.row + 1)
                    .chain(
                        context
                            .last_fixing_run_violations
                            .as_ref()
                            .unwrap()
                            .iter()
                            .map(|violation| {
                                edits_since_last_fixing_run.get_new_line_range(
                                    violation.range.start_byte..violation.range.end_byte,
                                )
                            }),
                    )
                    .collect_vec(),
            )
        }
        RunKind::FixingForSliceFixingLoop {
            all_violations_from_last_pass,
            all_fixes_from_last_pass,
            ..
        } => Some(
            all_violations_from_last_pass
                .into_iter()
                .map(|violation| violation.range.start_point.row..violation.range.end_point.row + 1)
                .chain(all_fixes_from_last_pass.into_iter().map(|(input_edit, _)| input_edit.start_position.row..input_edit.new_end_position.row + 1))
                .collect(),
        ),
        _ => None,
    };

    trace!(target: "rustfmt", ?line_ranges, run_kind = ?context.file_run_context.run_kind, "got line ranges");

    let mut args = vec![
        "+nightly".to_owned(),
        "--unstable-features".to_owned(),
        "--emit".to_owned(),
        "json".to_owned(),
    ];
    if let Some(line_ranges) = line_ranges {
        args.push("--file-lines".to_owned());
        args.push(
            serde_json::to_string(
                &line_ranges
                    .into_iter()
                    .map(|line_range| FileLineRange::new("stdin", line_range))
                    .collect_vec(),
            )
            .unwrap(),
        );
    }

    trace!(target: "rustfmt", "launching command");

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

    trace!(target: "rustfmt", "wait for output");

    let output = match command.wait_with_output() {
        Ok(output) => output,
        Err(err) => {
            trace!(target: "rustfmt", "Running rustfmt failed");

            eprintln!("Running rustfmt failed: {err}");
            return;
        }
    };

    trace!(target: "rustfmt", "Got output");

    if !output.status.success() {
        trace!(target: "rustfmt", "rustfmt returned an error");

        let err_str = String::from_utf8(output.stderr).expect("Couldn't parse stderr as utf8");
        eprintln!("rustfmt returned an error: {err_str}");
        return;
    }

    trace!(target: "rustfmt", "Deserializing JSON output");

    let files_with_mismatches: Vec<FileWithMismatches> =
        serde_json::from_str(std::str::from_utf8(&output.stdout).expect("Didn't get JSON output"))
            .unwrap_or_else(|_| {
                trace!(target: "rustfmt", "Couldn't deserialize JSON output");

                panic!("Couldn't deserialize JSON output");
            });

    if files_with_mismatches.is_empty() {
        trace!(target: "rustfmt", "No files with mismatches");

        return;
    }

    trace!(target: "rustfmt", ?files_with_mismatches, "Found files with mismatches");

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
            RopeOrSlice::Rope(rope) => {
                let start_byte = rope.line_to_byte(mismatch.original_begin_line - 1);
                let start_point = Point {
                    row: mismatch.original_begin_line - 1,
                    column: 0,
                };
                Range {
                    start_byte,
                    end_byte: if mismatch.original.is_empty() {
                        start_byte
                    } else {
                        rope.line_to_byte(mismatch.original_end_line)
                    },
                    start_point,
                    end_point: if mismatch.original.is_empty() {
                        start_point
                    } else {
                        Point {
                            row: mismatch.original_end_line,
                            column: 0,
                        }
                    },
                }
            }
            RopeOrSlice::Slice(slice) => {
                let newline_offsets = get_newline_offsets(slice).collect::<Vec<_>>();
                let start_byte = if mismatch.original_begin_line >= 2 {
                    newline_offsets
                        .get(mismatch.original_begin_line - 2)
                        .map_or(slice.len(), |&newline_offset| newline_offset + 1)
                } else {
                    0
                };
                let start_point = Point {
                    row: mismatch.original_begin_line - 1,
                    column: 0,
                };
                Range {
                    start_byte,
                    end_byte: if mismatch.original.is_empty() {
                        start_byte
                    } else {
                        newline_offsets
                            .get(mismatch.original_end_line - 1)
                            .map_or(slice.len(), |&newline_offset| newline_offset + 1)
                    },
                    start_point,
                    end_point: if mismatch.original.is_empty() {
                        start_point
                    } else {
                        Point {
                            row: mismatch.original_end_line,
                            column: 0,
                        }
                    },
                }
            }
        };

        trace!(target: "rustfmt", ?mismatch, ?range, "Reporting mismatch");

        context.report(violation! {
            node => node.descendant_for_byte_range(range.start_byte, range.end_byte).unwrap(),
            range => range,
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

#[derive(Debug, Deserialize)]
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

#[derive(Serialize)]
struct FileLineRange {
    file: &'static str,
    range: (usize, usize),
}

impl FileLineRange {
    fn new(file: &'static str, range: ops::Range<usize>) -> Self {
        Self {
            file,
            range: (range.start, range.end),
        }
    }
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
