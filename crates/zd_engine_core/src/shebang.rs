//! `#!` lines, with the kernel's semantics: the interpreter is the first
//! token, everything after it up to the newline is one optional
//! argument, and the line is limited to the first 256 bytes of the file
//! (`BINPRM_BUF_SIZE`).

pub const LINE_LIMIT: usize = 256;
pub const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shebang<'a> {
    pub interpreter: &'a [u8],
    pub argument: Option<&'a [u8]>,
}

/// Parse the start of a file. `None` when it does not begin with `#!`
/// or names no interpreter.
pub fn parse(head: &[u8]) -> Option<Shebang<'_>> {
    let rest = head.strip_prefix(b"#!")?;
    let limit = rest.len().min(LINE_LIMIT - 2);
    let line = &rest[..limit];
    let line = match line.iter().position(|&byte| byte == b'\n') {
        Some(end) => &line[..end],
        None => line,
    };
    let is_blank = |byte: &u8| *byte == b' ' || *byte == b'\t';
    let line = trim(line, is_blank);
    if line.is_empty() {
        return None;
    }
    let (interpreter, tail) = match line.iter().position(is_blank) {
        Some(end) => (&line[..end], &line[end..]),
        None => (line, &line[line.len()..]),
    };
    let argument = trim(tail, is_blank);
    Some(Shebang {
        interpreter,
        argument: if argument.is_empty() {
            None
        } else {
            Some(argument)
        },
    })
}

fn trim(bytes: &[u8], is_blank: impl Fn(&u8) -> bool) -> &[u8] {
    let start = bytes.iter().position(|byte| !is_blank(byte)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !is_blank(byte) && *byte != b'\r')
        .map(|index| index + 1)
        .unwrap_or(start);
    &bytes[start..end.max(start)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_interpreter() {
        let parsed = parse(b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(parsed.interpreter, b"/bin/sh");
        assert_eq!(parsed.argument, None);
    }

    #[test]
    fn env_with_argument_is_one_argument() {
        let parsed = parse(b"#! /usr/bin/env  python3 -u \nprint()").unwrap();
        assert_eq!(parsed.interpreter, b"/usr/bin/env");
        assert_eq!(parsed.argument, Some(&b"python3 -u"[..]));
    }

    #[test]
    fn rejects_non_scripts_and_empty_lines() {
        assert_eq!(parse(b"\x7fELF"), None);
        assert_eq!(parse(b"#!\n"), None);
        assert_eq!(parse(b"#!   \n"), None);
    }

    #[test]
    fn line_without_newline_and_crlf() {
        let parsed = parse(b"#!/usr/bin/perl").unwrap();
        assert_eq!(parsed.interpreter, b"/usr/bin/perl");
        let parsed = parse(b"#!/bin/bash -e\r\n").unwrap();
        assert_eq!(parsed.argument, Some(&b"-e"[..]));
    }

    #[test]
    fn line_is_cut_at_the_limit() {
        let mut head = Vec::from(&b"#!/bin/sh "[..]);
        head.extend(core::iter::repeat_n(b'x', 400));
        let parsed = parse(&head).unwrap();
        assert_eq!(parsed.interpreter, b"/bin/sh");
        assert_eq!(parsed.argument.unwrap().len(), LINE_LIMIT - 2 - b"/bin/sh ".len());
    }
}
