//! Shell parser
use failure::{bail, Fail, Fallible, format_err};
use shlex::string::ShellString;
use shlex::{Aliases, Environment, Expander, Lexer, Operator, ReservedWord, Token, TokenKind};

#[derive(Debug, Clone, Copy, Fail)]
pub enum ParseErrorKind {
    #[fail(display = "Unexpected token")]
    UnexpectedToken,
}

pub struct Parser<R: std::io::Read> {
    lexer: Lexer<R>,
    lookahead: Option<Token>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Separator {
    Sync,
    Async,
}

impl<R: std::io::Read> Parser<R> {
    pub fn new(source: &str, stream: R) -> Self {
        let lexer = Lexer::new(source, stream);
        Self {
            lexer,
            lookahead: None,
        }
    }

    pub fn parse(&mut self) -> Fallible<CompoundList> {
        let mut commands = vec![];

        let mut cmd = match self.and_or()? {
            Some(cmd) => cmd,
            None => {
                if self.next_token_is(TokenKind::Eof)? {
                    return Ok(CompoundList { commands });
                } else {
                    bail!("expected and_or");
                }
            }
        };

        cmd.asynchronous = self.separator_is_async()?;
        commands.push(cmd);

        while let Some(mut cmd) = self.and_or()? {
            cmd.asynchronous = self.separator_is_async()?;
            commands.push(cmd);
        }

        Ok(CompoundList { commands })
    }

    fn separator_is_async(&mut self) -> Fallible<bool> {
        Ok(self.separator()?.unwrap_or(Separator::Sync) == Separator::Async)
    }

    fn next_token(&mut self) -> Fallible<Token> {
        if let Some(tok) = self.lookahead.take() {
            Ok(tok)
        } else {
            self.lexer.next()
        }
    }

    fn unget_token(&mut self, tok: Token) {
        assert!(self.lookahead.is_none());
        self.lookahead.replace(tok);
    }

    fn pipeline_conditional(
        &mut self,
        condition: Pipeline,
        op: Operator,
    ) -> Fallible<Option<Command>> {
        self.linebreak()?;

        let then: CompoundList = Command::from(
            self.pipeline()?
                .ok_or_else(|| format_err!("missing pipeline after {:?}", op))?
        )
        .into();
        let condition: CompoundList = Command::from(condition).into();

        let (true_part, false_part) = if op == Operator::AndIf {
            (Some(then), None)
        } else {
            (None, Some(then))
        };

        Ok(Some(
            CommandType::If(If {
                condition: condition.into(),
                true_part,
                false_part,
            })
            .into(),
        ))
    }

    fn and_or(&mut self) -> Fallible<Option<Command>> {
        if let Some(pipeline) = self.pipeline()? {
            if self.next_token_is(TokenKind::Operator(Operator::AndIf))? {
                self.pipeline_conditional(pipeline, Operator::AndIf)
            } else if self.next_token_is(TokenKind::Operator(Operator::OrIf))? {
                self.pipeline_conditional(pipeline, Operator::OrIf)
            } else {
                Ok(Some(pipeline.into()))
            }
        } else {
            Ok(None)
        }
    }

    fn next_token_is_reserved_word(&mut self, word: ReservedWord) -> Fallible<bool> {
        let t = self.next_token()?;
        if t.is_reserved_word(word) {
            Ok(true)
        } else {
            self.unget_token(t);
            Ok(false)
        }
    }

    fn next_token_is(&mut self, kind: TokenKind) -> Fallible<bool> {
        let t = self.next_token()?;
        if kind == t.kind {
            Ok(true)
        } else {
            self.unget_token(t);
            Ok(false)
        }
    }

    fn pipeline(&mut self) -> Fallible<Option<Pipeline>> {
        let inverted = self.next_token_is_reserved_word(ReservedWord::Bang)?;
        if let Some(commands) = self.pipe_sequence()? {
            Ok(Some(Pipeline { inverted, commands }))
        } else if inverted {
            bail!("expected command to follow !");
        } else {
            Ok(None)
        }
    }

    fn pipe_sequence(&mut self) -> Fallible<Option<Vec<Command>>> {
        let command = match self.command()? {
            None => return Ok(None),
            Some(cmd) => cmd,
        };

        let mut commands = vec![command];

        while self.next_token_is(TokenKind::Operator(Operator::Pipe))? {
            self.linebreak()?;
            match self.command()? {
                Some(cmd) => commands.push(cmd),
                None => bail!("expected command to follow |"),
            }
        }

        Ok(Some(commands))
    }

    fn separator_op(&mut self) -> Fallible<Option<Separator>> {
        let t = self.next_token()?;
        match t.kind {
            TokenKind::Operator(Operator::Semicolon) => Ok(Some(Separator::Sync)),
            TokenKind::Operator(Operator::Ampersand) => Ok(Some(Separator::Async)),
            _ => {
                self.unget_token(t);
                Ok(None)
            }
        }
    }

    fn newline_list(&mut self) -> Fallible<Option<()>> {
        let mut saw_newline = false;
        loop {
            if self.next_token_is(TokenKind::NewLine)? {
                saw_newline = true
            } else if saw_newline {
                return Ok(Some(()));
            } else {
                return Ok(None);
            }
        }
    }

    fn linebreak(&mut self) -> Fallible<Option<()>> {
        self.newline_list()?;
        Ok(Some(()))
    }

    fn separator(&mut self) -> Fallible<Option<Separator>> {
        if let Some(sep) = self.separator_op()? {
            self.linebreak()?;
            Ok(Some(sep))
        } else if let Some(_) = self.newline_list()? {
            Ok(Some(Separator::Sync))
        } else {
            Ok(None)
        }
    }

    fn command(&mut self) -> Fallible<Option<Command>> {
        if let Some(command) = self.simple_command()? {
            Ok(Some(Command {
                command: CommandType::SimpleCommand(command),
                asynchronous: false,
                redirects: None,
            }))
        } else {
            Ok(None)
        }
    }

    fn simple_command(&mut self) -> Fallible<Option<SimpleCommand>> {
        let mut assignments = vec![];
        let mut words = vec![];
        let mut asynchronous = false;

        loop {
            let token = self.next_token()?;
            match token.kind {
                TokenKind::Eof => break,
                TokenKind::Operator(Operator::Ampersand) => {
                    asynchronous = true;
                    break;
                }
                TokenKind::Operator(Operator::Semicolon)
                | TokenKind::NewLine
                | TokenKind::Operator(Operator::AndIf)
                | TokenKind::Operator(Operator::OrIf) => {
                    self.unget_token(token);
                    break;
                }
                TokenKind::Word(_) => {
                    if words.is_empty() && token.kind.parse_assignment_word().is_some() {
                        assignments.push(token);
                    } else if words.is_empty() {
                        // Command word
                        // token.apply_command_word_rules(aliases);
                        words.push(token);
                    } else {
                        words.push(token);
                    }
                }

                _ => {
                    return Err(ParseErrorKind::UnexpectedToken.context(token.start).into());
                }
            }
        }

        if assignments.is_empty() && words.is_empty() {
            return Ok(None);
        }

        Ok(Some(SimpleCommand {
            assignments,
            file_redirects: vec![],
            fd_dups: vec![],
            words,
            asynchronous,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub asynchronous: bool,
    pub command: CommandType,
    pub redirects: Option<RedirectList>,
}

impl From<CommandType> for Command {
    fn from(command: CommandType) -> Command {
        Command {
            command,
            redirects: None,
            asynchronous: false,
        }
    }
}

impl From<Pipeline> for Command {
    fn from(pipeline: Pipeline) -> Command {
        // Simplify a pipeline to the command itself if possible
        if !pipeline.inverted && pipeline.commands.len() == 1 {
            pipeline.commands.into_iter().next().unwrap()
        } else {
            CommandType::Pipeline(pipeline).into()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectList {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandType {
    Pipeline(Pipeline),
    SimpleCommand(SimpleCommand),
    BraceGroup(CompoundList),
    Subshell(CompoundList),
    ForEach(ForEach),
    If(If),
    UntilLoop(UntilLoop),
    WhileLoop(WhileLoop),
    // TODO: Case
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    /// true if the pipeline starts with a bang
    inverted: bool,
    commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompoundList {
    commands: Vec<Command>,
}

impl IntoIterator for CompoundList {
    type Item = Command;
    type IntoIter = ::std::vec::IntoIter<Command>;
    fn into_iter(self) -> Self::IntoIter {
        self.commands.into_iter()
    }
}

impl From<Command> for CompoundList {
    fn from(cmd: Command) -> CompoundList {
        CompoundList {
            commands: vec![cmd],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct If {
    condition: CompoundList,
    true_part: Option<CompoundList>,
    false_part: Option<CompoundList>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntilLoop {
    body: CompoundList,
    condition: CompoundList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhileLoop {
    condition: CompoundList,
    body: CompoundList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForEach {
    wordlist: Vec<Token>,
    body: CompoundList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRedirection {
    pub fd_number: usize,
    pub file_name: Token,
    /// `<` or `<>`
    pub input: bool,
    /// `>` or `<>`
    pub output: bool,
    /// `>|`
    pub clobber: bool,
    /// `>>`
    pub append: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FdDuplication {
    /// Dup `src_fd_number` ...
    pub src_fd_number: usize,
    /// ... into `dest_fd_number` for the child
    pub dest_fd_number: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// Any assignment words to override the environment
    assignments: Vec<Token>,
    file_redirects: Vec<FileRedirection>,
    fd_dups: Vec<FdDuplication>,
    /// The words that will be expanded to form the argv
    words: Vec<Token>,
    /// true if `&` was used as the separator between
    /// commands in the containing list
    asynchronous: bool,
}

impl SimpleCommand {
    pub fn expand_argv(
        &self,
        env: &mut Environment,
        expander: &Expander,
        aliases: &Aliases,
    ) -> Fallible<Vec<ShellString>> {
        // FIXME: scoped assignments need to return a new env
        let mut argv = vec![];
        for word in &self.words {
            let word = if argv.is_empty() {
                let mut word = word.clone();
                word.apply_command_word_rules(Some(aliases));
                word
            } else {
                word.clone()
            };

            match word.kind {
                TokenKind::Word(ref s) | TokenKind::Name(ref s) => {
                    let mut fields = expander.expand_word(&s.as_str().into(), env)?;
                    argv.append(&mut fields);
                }
                _ => bail!("unhandled token kind {:?}", word),
            }
        }
        Ok(argv)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use pretty_assertions::assert_eq;
    use shlex::TokenPosition;

    fn parse(text: &str) -> Fallible<CompoundList> {
        let mut parser = Parser::new("test", text.as_bytes());
        parser.parse()
    }

    #[test]
    fn test_parse() {
        let list = parse("ls -l foo").unwrap();
        assert_eq!(
            list,
            CompoundList {
                commands: vec![Command::from(CommandType::SimpleCommand(SimpleCommand {
                    assignments: vec![],
                    file_redirects: vec![],
                    fd_dups: vec![],
                    asynchronous: false,
                    words: vec![
                        Token::new(
                            TokenKind::Word("ls".to_string()),
                            TokenPosition {
                                line_number: 0,
                                col_number: 0
                            },
                            TokenPosition {
                                line_number: 0,
                                col_number: 1
                            },
                        ),
                        Token::new(
                            TokenKind::Word("-l".to_string()),
                            TokenPosition {
                                line_number: 0,
                                col_number: 3
                            },
                            TokenPosition {
                                line_number: 0,
                                col_number: 4
                            },
                        ),
                        Token::new(
                            TokenKind::Word("foo".to_string()),
                            TokenPosition {
                                line_number: 0,
                                col_number: 6
                            },
                            TokenPosition {
                                line_number: 0,
                                col_number: 8
                            },
                        )
                    ]
                }),)]
            }
        );
    }

    #[test]
    fn test_parse_two_lines() {
        let list = parse("false\ntrue").unwrap();
        assert_eq!(
            list,
            CompoundList {
                commands: vec![
                    Command::from(CommandType::SimpleCommand(SimpleCommand {
                        assignments: vec![],
                        file_redirects: vec![],
                        fd_dups: vec![],
                        asynchronous: false,
                        words: vec![Token::new(
                            TokenKind::Word("false".to_string()),
                            TokenPosition {
                                line_number: 0,
                                col_number: 0
                            },
                            TokenPosition {
                                line_number: 0,
                                col_number: 4
                            },
                        ),]
                    }),),
                    Command::from(CommandType::SimpleCommand(SimpleCommand {
                        assignments: vec![],
                        file_redirects: vec![],
                        fd_dups: vec![],
                        asynchronous: false,
                        words: vec![Token::new(
                            TokenKind::Word("true".to_string()),
                            TokenPosition {
                                line_number: 1,
                                col_number: 0
                            },
                            TokenPosition {
                                line_number: 1,
                                col_number: 3
                            },
                        ),]
                    }))
                ]
            }
        );
    }

    #[test]
    fn test_parse_with_alias() {
        let list = parse("ls foo").unwrap();
        let mut aliases = Aliases::new();
        aliases.alias("ls", "ls -l");
        let mut env = Environment::new();
        struct MockExpander {}
        impl Expander for MockExpander {
            fn lookup_homedir(
                &self,
                _user: Option<&str>,
                _env: &mut Environment,
            ) -> Fallible<ShellString> {
                bail!("nope");
            }
        }
        if let Command {
            command: CommandType::SimpleCommand(cmd),
            ..
        } = &list.commands[0]
        {
            let argv = cmd
                .expand_argv(&mut env, &MockExpander {}, &aliases)
                .unwrap();
            assert_eq!(
                argv,
                vec![
                    "ls".to_string().into(),
                    "-l".to_string().into(),
                    "foo".to_string().into()
                ]
            );
        } else {
            panic!("wrong command type!?");
        }
    }
}
