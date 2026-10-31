//! The shell: a line editor and a command parser, both of which can be tested.
//!
//! # Why the parser is separate
//!
//! What a command does needs a filesystem, a desktop and a scheduler; what a line *means*
//! needs nothing but the line. Splitting them is what lets the host tests cover the awkward
//! parts - an empty line, a command nobody has, a number that is not one, a `write` whose text
//! contains spaces - without a machine, and keeps the executor in `main.rs` to a match over
//! things that are already proven.
//!
//! # The editor
//!
//! A line buffer with a cursor, a backspace that stays on character boundaries, and a history
//! that `up` and `down` walk. It takes [`Key`]s and returns an [`Action`], so a test types a
//! line by feeding keys and reads the submitted text back, exactly as the keyboard's interrupt
//! handler will.

use alloc::string::String;
use alloc::vec::Vec;

use crate::keyboard::Key;

/// One thing the user asked for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Command {
    /// The list of commands.
    Help,
    /// Prints its argument.
    Echo(String),
    /// Lists the filesystem.
    List,
    /// Prints a file.
    Cat(String),
    /// Writes a file, creating it or replacing it.
    Write { name: String, text: String },
    /// Removes a file.
    Remove(String),
    /// The frame allocator and the arena.
    Memory,
    /// The timer and the task table.
    Ticks,
    /// The scheduler's tasks.
    Tasks,
    /// The windows and which one has focus.
    Windows,
    /// What the firmware's tables say the machine is.
    Cpus,
    /// Runs a program from the filesystem.
    Run(String),
    /// Raises a window by id.
    Raise(u32),
    /// Closes a window by id.
    Close(u32),
    /// Clears the shell's own window.
    Clear,
    /// Ends the session.
    Quit,
}

/// Why a line could not be parsed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// A command that needs an argument did not get one.
    Missing(&'static str),
    /// A command this shell does not have.
    Unknown(String),
    /// A number that is not one.
    BadNumber(String),
}

/// Parses one line into a command, or `None` for a line that asks for nothing.
///
/// Splitting on whitespace is the whole grammar: there is no quoting, so `write notes hello
/// world` writes `hello world`. A shell that needed quoting to say a sentence would be a shell
/// nobody could use with this keyboard yet.
pub fn parse(line: &str) -> Result<Option<Command>, Error> {
    let mut words = line.split_whitespace();
    let Some(name) = words.next() else {
        return Ok(None);
    };
    let name = name.to_ascii_lowercase();
    let rest: Vec<&str> = words.collect();
    let joined = rest.join(" ");
    let id = |word: Option<&str>| -> Result<u32, Error> {
        let word = word.ok_or(Error::Missing("a window id"))?;
        word.parse()
            .map_err(|_| Error::BadNumber(String::from(word)))
    };
    let command = match name.as_str() {
        "help" | "?" => Command::Help,
        "echo" => Command::Echo(joined),
        "ls" | "list" | "dir" => Command::List,
        "cat" | "type" => Command::Cat(single(&rest, "a file name")?),
        "write" => {
            let name = rest.first().ok_or(Error::Missing("a file name"))?;
            // Everything after the name is the text, spaces included.
            let text = rest.get(1..).unwrap_or_default().join(" ");
            Command::Write {
                name: String::from(*name),
                text,
            }
        }
        "rm" | "remove" | "delete" => Command::Remove(single(&rest, "a file name")?),
        "mem" | "memory" | "free" => Command::Memory,
        "ticks" | "uptime" => Command::Ticks,
        "tasks" | "ps" => Command::Tasks,
        "windows" | "win" => Command::Windows,
        "cpus" | "cpu" => Command::Cpus,
        "run" | "exec" => Command::Run(single(&rest, "a program name")?),
        "raise" | "focus" => Command::Raise(id(rest.first().copied())?),
        "close" => Command::Close(id(rest.first().copied())?),
        "clear" | "cls" => Command::Clear,
        "exit" | "quit" | "halt" => Command::Quit,
        other => return Err(Error::Unknown(String::from(other))),
    };
    Ok(Some(command))
}

/// The single argument a command takes, or the error that says it is missing.
fn single(words: &[&str], what: &'static str) -> Result<String, Error> {
    let word = words.first().ok_or(Error::Missing(what))?;
    Ok(String::from(*word))
}

/// The lines `help` prints: each command with its arguments.
pub const HELP: &[&str] = &[
    "help              this list",
    "echo <text>       print the text",
    "ls                list the filesystem",
    "cat <name>        print a file",
    "write <name> <t>  write a file",
    "rm <name>         remove a file",
    "mem               frames and heap",
    "ticks             the timer and the tasks",
    "tasks             the scheduler's table",
    "windows           the windows and the focus",
    "cpus              the firmware's cpu and interrupt tables",
    "run <name>        run a program from the disk",
    "raise <id>        bring a window to the front",
    "close <id>        close a window",
    "clear             empty this window",
    "exit              end the session",
];

/// What a key did to the line.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// The line changed; the caller should draw it again.
    Edited,
    /// Nothing the caller has to redraw.
    Idle,
    /// The line was submitted, and is now its own history entry.
    Submit(String),
    /// The line was abandoned.
    Cancelled,
}

/// The line being typed, and the history of lines already submitted.
pub struct Editor {
    line: String,
    /// Where the next character goes, in bytes and always on a character boundary.
    cursor: usize,
    history: Vec<String>,
    /// How far back in the history the line came from, or `None` when it is being typed.
    recall: Option<usize>,
}

impl Editor {
    /// An empty line and an empty history.
    pub fn new() -> Self {
        Self {
            line: String::new(),
            cursor: 0,
            history: Vec::new(),
            recall: None,
        }
    }

    /// The line as it stands, for the prompt and the cursor.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Where the cursor is, as a byte offset into [`Self::line`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Applies one keystroke.
    pub fn handle(&mut self, key: Key) -> Action {
        match key {
            Key::Char(character) => {
                self.line.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                self.recall = None;
                Action::Edited
            }
            Key::Backspace => self.delete_before(),
            Key::Delete => {
                if self.cursor >= self.line.len() {
                    return Action::Idle;
                }
                let mut end = self.cursor + 1;
                while end < self.line.len() && !self.line.is_char_boundary(end) {
                    end += 1;
                }
                self.line.replace_range(self.cursor..end, "");
                Action::Edited
            }
            Key::Left => {
                let Some(start) = self.previous_boundary(self.cursor) else {
                    return Action::Idle;
                };
                self.cursor = start;
                Action::Edited
            }
            Key::Right => {
                if self.cursor >= self.line.len() {
                    return Action::Idle;
                }
                let mut end = self.cursor + 1;
                while end < self.line.len() && !self.line.is_char_boundary(end) {
                    end += 1;
                }
                self.cursor = end;
                Action::Edited
            }
            Key::Home => {
                self.cursor = 0;
                Action::Edited
            }
            Key::End => {
                self.cursor = self.line.len();
                Action::Edited
            }
            Key::Up => self.recall_older(),
            Key::Down => self.recall_newer(),
            Key::Escape => {
                self.line.clear();
                self.cursor = 0;
                self.recall = None;
                Action::Cancelled
            }
            Key::Enter => {
                let line = core::mem::take(&mut self.line);
                self.cursor = 0;
                self.recall = None;
                if !line.trim().is_empty() {
                    self.history.push(line.clone());
                }
                Action::Submit(line)
            }
            // Tab, the page keys and everything else mean nothing here yet.
            _ => Action::Idle,
        }
    }

    /// Removes the character before the cursor, which may be several bytes wide.
    fn delete_before(&mut self) -> Action {
        let Some(start) = self.previous_boundary(self.cursor) else {
            return Action::Idle;
        };
        self.line.replace_range(start..self.cursor, "");
        self.cursor = start;
        Action::Edited
    }

    /// The character boundary before `from`, or `None` at the start of the line.
    fn previous_boundary(&self, from: usize) -> Option<usize> {
        if from == 0 {
            return None;
        }
        let mut start = from - 1;
        while start > 0 && !self.line.is_char_boundary(start) {
            start -= 1;
        }
        Some(start)
    }

    /// Replaces the line with an older history entry.
    fn recall_older(&mut self) -> Action {
        let index = match self.recall {
            Some(0) => return Action::Idle,
            Some(index) => index - 1,
            None => match self.history.len() {
                0 => return Action::Idle,
                len => len - 1,
            },
        };
        self.recall = Some(index);
        self.line = self.history[index].clone();
        self.cursor = self.line.len();
        Action::Edited
    }

    /// Walks forward through the history, and off its end into an empty line.
    fn recall_newer(&mut self) -> Action {
        let Some(index) = self.recall else {
            return Action::Idle;
        };
        if index + 1 >= self.history.len() {
            self.recall = None;
            self.line.clear();
            self.cursor = 0;
            return Action::Edited;
        }
        self.recall = Some(index + 1);
        self.line = self.history[index + 1].clone();
        self.cursor = self.line.len();
        Action::Edited
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Types a line one character at a time and submits it.
    fn type_and_submit(editor: &mut Editor, line: &str) -> String {
        for character in line.chars() {
            editor.handle(Key::Char(character));
        }
        match editor.handle(Key::Enter) {
            Action::Submit(text) => text,
            other => panic!("expected a submission, got {other:?}"),
        }
    }

    #[test]
    fn every_command_parses_with_its_arguments() {
        let cases: Vec<(&str, Command)> = vec![
            ("help", Command::Help),
            ("?", Command::Help),
            ("echo", Command::Echo(String::new())),
            ("ls", Command::List),
            ("cat motd", Command::Cat(String::from("motd"))),
            (
                "write notes hello world",
                Command::Write {
                    name: String::from("notes"),
                    text: String::from("hello world"),
                },
            ),
            ("rm notes", Command::Remove(String::from("notes"))),
            ("mem", Command::Memory),
            ("ticks", Command::Ticks),
            ("tasks", Command::Tasks),
            ("windows", Command::Windows),
            ("cpus", Command::Cpus),
            (
                "run apps/fib.gvm",
                Command::Run(String::from("apps/fib.gvm")),
            ),
            ("raise 3", Command::Raise(3)),
            ("close 12", Command::Close(12)),
            ("clear", Command::Clear),
            ("exit", Command::Quit),
        ];
        for (line, expected) in cases {
            assert_eq!(parse(line), Ok(Some(expected)), "line {line:?}");
        }
    }

    #[test]
    fn commands_are_case_insensitive_and_extra_space_does_not_matter() {
        assert_eq!(parse("  LS  "), Ok(Some(Command::List)));
        assert_eq!(
            parse("ECHO  hi"),
            Ok(Some(Command::Echo(String::from("hi"))))
        );
        assert_eq!(
            parse("cat   motd"),
            Ok(Some(Command::Cat(String::from("motd"))))
        );
    }

    #[test]
    fn an_empty_line_asks_for_nothing_rather_than_failing() {
        assert_eq!(parse(""), Ok(None));
        assert_eq!(parse("   "), Ok(None));
        assert_eq!(parse("\t"), Ok(None));
    }

    #[test]
    fn a_missing_or_wrong_argument_says_which() {
        assert_eq!(parse("cat"), Err(Error::Missing("a file name")));
        assert_eq!(parse("write"), Err(Error::Missing("a file name")));
        assert_eq!(parse("raise"), Err(Error::Missing("a window id")));
        assert_eq!(parse("run"), Err(Error::Missing("a program name")));
        assert_eq!(
            parse("raise three"),
            Err(Error::BadNumber(String::from("three")))
        );
        assert_eq!(parse("close -1"), Err(Error::BadNumber(String::from("-1"))));
    }

    #[test]
    fn a_command_nobody_has_is_named_back() {
        assert_eq!(
            parse("frobnicate"),
            Err(Error::Unknown(String::from("frobnicate")))
        );
    }

    #[test]
    fn echoing_joins_the_words_after_the_command() {
        assert_eq!(
            parse("echo one two  three"),
            Ok(Some(Command::Echo(String::from("one two three"))))
        );
    }

    #[test]
    fn writing_a_file_keeps_the_rest_of_the_line_as_text() {
        assert_eq!(
            parse("write motd hello from grey os"),
            Ok(Some(Command::Write {
                name: String::from("motd"),
                text: String::from("hello from grey os"),
            }))
        );
        // A file with no text is legitimate.
        assert_eq!(
            parse("write empty"),
            Ok(Some(Command::Write {
                name: String::from("empty"),
                text: String::new(),
            }))
        );
    }

    #[test]
    fn typing_builds_a_line_and_enter_submits_it() {
        let mut editor = Editor::new();
        for character in "ls".chars() {
            assert_eq!(editor.handle(Key::Char(character)), Action::Edited);
        }
        assert_eq!(editor.line(), "ls");
        assert_eq!(editor.cursor(), 2);
        assert_eq!(
            editor.handle(Key::Enter),
            Action::Submit(String::from("ls"))
        );
        assert_eq!(editor.line(), "", "the line was not cleared");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn backspace_removes_one_character_and_stops_at_the_start() {
        let mut editor = Editor::new();
        type_and_submit(&mut editor, "cat");
        for character in "ab".chars() {
            editor.handle(Key::Char(character));
        }
        assert_eq!(editor.handle(Key::Backspace), Action::Edited);
        assert_eq!(editor.line(), "a");
        assert_eq!(editor.handle(Key::Backspace), Action::Edited);
        assert_eq!(editor.line(), "");
        assert_eq!(editor.handle(Key::Backspace), Action::Idle);
    }

    #[test]
    fn backspace_on_a_multi_byte_character_removes_the_whole_character() {
        let mut editor = Editor::new();
        editor.handle(Key::Char('α'));
        assert_eq!(editor.cursor(), 2, "alpha is two bytes");
        assert_eq!(editor.handle(Key::Backspace), Action::Edited);
        assert_eq!(editor.line(), "");
        assert_eq!(editor.cursor(), 0);
        // And the same character in the middle of a line.
        for character in "aβc".chars() {
            editor.handle(Key::Char(character));
        }
        editor.handle(Key::Left);
        assert_eq!(editor.handle(Key::Backspace), Action::Edited);
        assert_eq!(editor.line(), "ac");
    }

    #[test]
    fn the_arrows_home_and_end_move_the_cursor_and_insert_where_it_is() {
        let mut editor = Editor::new();
        for character in "ac".chars() {
            editor.handle(Key::Char(character));
        }
        editor.handle(Key::Left);
        editor.handle(Key::Char('b'));
        assert_eq!(editor.line(), "abc");
        editor.handle(Key::Home);
        assert_eq!(editor.cursor(), 0);
        editor.handle(Key::Char('>'));
        assert_eq!(editor.line(), ">abc");
        editor.handle(Key::End);
        assert_eq!(editor.cursor(), 4);
        // Right at the end is idle rather than moving past it.
        assert_eq!(editor.handle(Key::Right), Action::Idle);
        editor.handle(Key::Home);
        assert_eq!(editor.handle(Key::Left), Action::Idle);
    }

    #[test]
    fn the_history_walks_back_and_forward_and_ends_empty() {
        let mut editor = Editor::new();
        type_and_submit(&mut editor, "ls");
        type_and_submit(&mut editor, "mem");
        assert_eq!(editor.handle(Key::Up), Action::Edited);
        assert_eq!(editor.line(), "mem");
        assert_eq!(editor.handle(Key::Up), Action::Edited);
        assert_eq!(editor.line(), "ls");
        // Older than the oldest entry: idle, and the line stays.
        assert_eq!(editor.handle(Key::Up), Action::Idle);
        assert_eq!(editor.line(), "ls");
        assert_eq!(editor.handle(Key::Down), Action::Edited);
        assert_eq!(editor.line(), "mem");
        // Past the newest entry is an empty line rather than a stuck one.
        assert_eq!(editor.handle(Key::Down), Action::Edited);
        assert_eq!(editor.line(), "");
        assert_eq!(editor.handle(Key::Down), Action::Idle);
    }

    #[test]
    fn escape_abandons_the_line_without_remembering_it() {
        let mut editor = Editor::new();
        for character in "half typed".chars() {
            editor.handle(Key::Char(character));
        }
        assert_eq!(editor.handle(Key::Escape), Action::Cancelled);
        assert_eq!(editor.line(), "");
        // Nothing was recorded, so `up` finds nothing.
        assert_eq!(editor.handle(Key::Up), Action::Idle);
    }

    #[test]
    fn an_empty_submission_is_not_remembered() {
        let mut editor = Editor::new();
        editor.handle(Key::Enter);
        assert_eq!(editor.handle(Key::Up), Action::Idle);
        // Whitespace alone counts as empty.
        for character in "   ".chars() {
            editor.handle(Key::Char(character));
        }
        assert_eq!(
            editor.handle(Key::Enter),
            Action::Submit(String::from("   "))
        );
        assert_eq!(editor.handle(Key::Up), Action::Idle);
    }

    #[test]
    fn the_help_list_names_every_command_the_parser_accepts() {
        // Every name in the help text has to be a command the parser knows. Some of them need
        // an argument, which the bare name does not have, so the claim is "not unknown"
        // rather than "parses".
        for line in HELP {
            let name = line.split_whitespace().next().expect("a command name");
            assert!(
                !matches!(parse(name), Err(Error::Unknown(_))),
                "the help lists `{name}` but the parser does not have it"
            );
        }
    }
}
