//! Hand-rolled recursive-descent parser for the Datalog-v1 surface syntax.
//!
//! Grammar:
//! ```text
//! program ::= clause*
//! clause  ::= atom ("." | ":-" atom ("," atom)* ".")
//! atom    ::= ident "(" term ("," term)* ")"
//! term    ::= VAR | CONST
//! VAR     ::= [A-Z_] [A-Za-z0-9_]*
//! CONST   ::= ident | quoted_string | integer
//! ident   ::= [a-z] [A-Za-z0-9_]*
//! ```
//!
//! Line comments begin with `%` or `//`.

use thiserror::Error;

use crate::ast::{Atom, Program, Rule, Term};

#[derive(Debug, Error)]
#[error("parse error at line {line}, col {col}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

pub fn parse(src: &str) -> Result<Program, ParseError> {
    let mut p = Parser::new(src);
    let mut program = Program::default();
    p.skip_trivia();
    while !p.eof() {
        let rule = p.parse_clause()?;
        if rule.is_ground_fact() {
            program.facts.push(rule.head);
        } else {
            program.rules.push(rule);
        }
        p.skip_trivia();
    }
    Ok(program)
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(b)
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError {
            line: self.line,
            col: self.col,
            message: msg.into(),
        }
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.bump();
                }
                Some(b'%') => self.skip_line_comment(),
                Some(b'/') if self.src.get(self.pos + 1) == Some(&b'/') => {
                    self.skip_line_comment();
                }
                _ => break,
            }
        }
    }

    fn skip_line_comment(&mut self) {
        while let Some(b) = self.peek() {
            self.bump();
            if b == b'\n' {
                break;
            }
        }
    }

    fn eat(&mut self, c: u8) -> Result<(), ParseError> {
        match self.peek() {
            Some(b) if b == c => {
                self.bump();
                Ok(())
            }
            Some(other) => Err(self.err(format!(
                "expected `{}`, found `{}`",
                c as char, other as char
            ))),
            None => Err(self.err(format!("expected `{}`, found EOF", c as char))),
        }
    }

    fn parse_clause(&mut self) -> Result<Rule, ParseError> {
        let head = self.parse_atom()?;
        self.skip_trivia();
        if self.peek() == Some(b'.') {
            self.bump();
            return Ok(Rule {
                head,
                body: Vec::new(),
            });
        }
        // `:-`
        self.eat(b':')?;
        self.eat(b'-')?;
        self.skip_trivia();
        let mut body = vec![self.parse_atom()?];
        loop {
            self.skip_trivia();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                    self.skip_trivia();
                    body.push(self.parse_atom()?);
                }
                Some(b'.') => {
                    self.bump();
                    break;
                }
                Some(c) => {
                    return Err(self.err(format!("expected `,` or `.`, found `{}`", c as char)))
                }
                None => return Err(self.err("unexpected EOF in clause body")),
            }
        }
        Ok(Rule { head, body })
    }

    fn parse_atom(&mut self) -> Result<Atom, ParseError> {
        let predicate = self.parse_ident_lower()?;
        self.skip_trivia();
        self.eat(b'(')?;
        self.skip_trivia();
        let mut args = Vec::new();
        if self.peek() != Some(b')') {
            args.push(self.parse_term()?);
            loop {
                self.skip_trivia();
                match self.peek() {
                    Some(b',') => {
                        self.bump();
                        self.skip_trivia();
                        args.push(self.parse_term()?);
                    }
                    Some(b')') => break,
                    Some(c) => {
                        return Err(self.err(format!("expected `,` or `)`, found `{}`", c as char)))
                    }
                    None => return Err(self.err("unexpected EOF in atom args")),
                }
            }
        }
        self.eat(b')')?;
        Ok(Atom { predicate, args })
    }

    fn parse_term(&mut self) -> Result<Term, ParseError> {
        match self.peek() {
            Some(b'"') => Ok(Term::Const(self.parse_quoted()?)),
            Some(b) if b.is_ascii_digit() || b == b'-' => Ok(Term::Const(self.parse_number()?)),
            Some(b) if b.is_ascii_uppercase() || b == b'_' => {
                let name = self.parse_ident_any()?;
                Ok(Term::Var(name))
            }
            Some(b) if b.is_ascii_lowercase() => {
                let name = self.parse_ident_any()?;
                Ok(Term::Const(name))
            }
            Some(c) => Err(self.err(format!("unexpected character `{}` in term", c as char))),
            None => Err(self.err("unexpected EOF in term")),
        }
    }

    fn parse_ident_lower(&mut self) -> Result<String, ParseError> {
        match self.peek() {
            Some(b) if b.is_ascii_lowercase() => self.parse_ident_any(),
            Some(c) => Err(self.err(format!("expected predicate name, found `{}`", c as char))),
            None => Err(self.err("expected predicate name, found EOF")),
        }
    }

    fn parse_ident_any(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.bump();
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(self.err("expected identifier"));
        }
        Ok(std::str::from_utf8(&self.src[start..self.pos])
            .unwrap()
            .to_string())
    }

    fn parse_quoted(&mut self) -> Result<String, ParseError> {
        self.eat(b'"')?;
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b == b'"' {
                let s = std::str::from_utf8(&self.src[start..self.pos])
                    .unwrap()
                    .to_string();
                self.bump();
                return Ok(s);
            }
            if b == b'\\' {
                self.bump();
            }
            self.bump();
        }
        Err(self.err("unterminated string literal"))
    }

    fn parse_number(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        if start == self.pos {
            return Err(self.err("expected number"));
        }
        Ok(std::str::from_utf8(&self.src[start..self.pos])
            .unwrap()
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_and_rule() {
        let src = r#"
            % a comment
            parent(alice, bob).
            parent(bob, carol).
            ancestor(X, Y) :- parent(X, Y).
            ancestor(X, Z) :- parent(X, Y), ancestor(Y, Z).
        "#;
        let p = parse(src).unwrap();
        assert_eq!(p.facts.len(), 2);
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.rules[0].body.len(), 1);
        assert_eq!(p.rules[1].body.len(), 2);
    }

    #[test]
    fn quoted_and_number() {
        let src = r#"p("hello world", 42)."#;
        let p = parse(src).unwrap();
        assert_eq!(p.facts.len(), 1);
        match &p.facts[0].args[0] {
            Term::Const(s) => assert_eq!(s, "hello world"),
            _ => panic!(),
        }
    }

    #[test]
    fn reports_error_location() {
        let err = parse("parent(alice bob).").unwrap_err();
        assert!(err.message.contains("expected"));
    }
}
