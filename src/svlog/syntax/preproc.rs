// Copyright (c) 2016-2017 Fabian Schuiki

//! A preprocessor for SystemVerilog files that takes the raw stream of
//! tokens generated by a lexer and performs include and macro
//! resolution.

use crate::cat::*;
use moore_common::errors::{DiagBuilder2, DiagResult2};
use moore_common::source::*;
use std::{collections::HashMap, fmt, path::Path, rc::Rc};

type TokenAndSpan = (CatTokenKind, Span);

pub struct Preprocessor<'a> {
    /// The stack of input files. Tokens are taken from the topmost stream until
    /// the end of input, at which point the stream is popped and the process
    /// continues with the next stream. Used to handle include files.
    stack: Vec<Stream<'a>>,
    /// References to the source contents that were touched by the preprocessor.
    /// Keeping these around ensures that all emitted tokens remain valid (and
    /// point to valid memory locations) at least until the preprocessor is
    /// dropped.
    contents: Vec<Rc<dyn SourceContent>>,
    /// The current token, or None if either the end of the stream has been
    /// encountered, or at the beginning when no token has been read yet.
    token: Option<TokenAndSpan>,
    /// The defined macros.
    macro_defs: HashMap<String, Macro>,
    /// The stack used to inject expanded macros into the token stream.
    macro_stack: Vec<TokenAndSpan>,
    /// The paths that are searched for included files, besides the current
    /// file's directory.
    include_paths: &'a [&'a Path],
    /// The define conditional stack. Whenever a `ifdef, `ifndef, `else, `elsif,
    /// or `endif directive is encountered, the stack is expanded, modified, or
    /// reduced to reflect the kind of conditional block we're in.
    defcond_stack: Vec<Defcond>,
    /// Currently enabled directives.
    dirs: Directives,
}

impl<'a> Preprocessor<'a> {
    /// Create a new preprocessor for the given source file.
    pub fn new(
        source: Source,
        include_paths: &'a [&'a Path],
        macro_defs: &'a [(&'a str, Option<&'a str>)],
    ) -> Preprocessor<'a> {
        let content = source.get_content();
        let content_unbound = unsafe { &*(content.as_ref() as *const dyn SourceContent) };
        let iter = content_unbound.iter();
        let macro_defs = macro_defs
            .into_iter()
            .map(|(name, value)| {
                let body = match value {
                    Some(value) => {
                        // Create dummy sources for each user defined macro.
                        let src = get_source_manager().add_anonymous(*value);
                        let span = Span::new(src, 0, value.len());
                        Cat::new(Box::new(value.char_indices()))
                            .map(|x| (x.0, span))
                            .collect()
                    }
                    None => Vec::new(),
                };
                (
                    name.to_string(),
                    Macro {
                        name: name.to_string(),
                        span: INVALID_SPAN,
                        args: Vec::new(),
                        body: body,
                    },
                )
            })
            .collect();
        Preprocessor {
            stack: vec![Stream {
                source: source,
                iter: Cat::new(iter),
            }],
            contents: vec![content],
            token: None,
            macro_defs,
            macro_stack: Vec::new(),
            include_paths: include_paths,
            defcond_stack: Vec::new(),
            dirs: Default::default(),
        }
    }

    /// Advance to the next token in the input stream.
    fn bump(&mut self) {
        self.token = self.macro_stack.pop();
        if self.token.is_some() {
            return;
        }
        loop {
            self.token = match self.stack.last_mut() {
                Some(stream) => stream
                    .iter
                    .next()
                    .map(|tkn| (tkn.0, Span::new(stream.source, tkn.1, tkn.2))),
                None => return,
            };
            if self.token.is_none() {
                self.stack.pop();
            } else {
                break;
            }
        }
    }

    /// Called whenever we have encountered a backtick followed by a text token.
    /// This function handles all compiler directives and performs file
    /// inclusion and macro expansion.
    fn handle_directive<S: AsRef<str>>(&mut self, dir_name: S, span: Span) -> DiagResult2<()> {
        let dir_name = dir_name.as_ref();
        let dir = DIRECTIVES_TABLE
            .with(|tbl| tbl.get(dir_name).map(|x| *x).unwrap_or(Directive::Unknown));

        match dir {
            Directive::Include => {
                if self.is_inactive() {
                    return Ok(());
                }

                // Skip leading whitespace.
                match self.token {
                    Some((Whitespace, _)) => self.bump(),
                    _ => (),
                }

                // Match the opening double quotes or angular bracket.
                let name_p;
                let name_q;
                let closing = match self.token {
                    Some((Symbol('"'), sp)) => { name_p = sp.end(); self.bump(); '"' },
                    Some((Symbol('<'), sp)) => { name_p = sp.end(); self.bump(); '>' },
                    _ => { return Err(DiagBuilder2::fatal("expected filename inside double quotes (\"...\") or angular brackets (<...>) after `include").span(span))}
                };

                // Accumulate the include path until the closing symbol.
                let mut filename = String::new();
                loop {
                    match self.token {
                        Some((Symbol(c), sp)) if c == closing => {
                            name_q = sp.begin();
                            break;
                        }
                        Some((Newline, sp)) => {
                            return Err(DiagBuilder2::fatal(
                                "expected end of included file's name before line break",
                            )
                            .span(sp));
                        }
                        Some((_, sp)) => {
                            filename.push_str(&sp.extract());
                            self.bump();
                        }
                        None => {
                            return Err(DiagBuilder2::fatal("expected filename after `include directive before the end of the input").span(span));
                        }
                    }
                }

                // Create a new lexer for the included filename and push it onto the
                // stream stack.
                // TODO: Search only system location if `include <...> is used
                let included_source = match self.open_include(&filename, &span.source.get_path()) {
                    Some(src) => src,
                    None => {
                        // TODO: Add notes to the message indicating which files have been tried.
                        return Err(DiagBuilder2::fatal(format!(
                            "cannot open included file \"{}\"",
                            filename
                        ))
                        .span(Span::union(name_p, name_q)));
                    }
                };

                let content = included_source.get_content();
                let content_unbound = unsafe { &*(content.as_ref() as *const dyn SourceContent) };
                let iter = content_unbound.iter();
                self.contents.push(content);
                self.stack.push(Stream {
                    source: included_source,
                    iter: Cat::new(iter),
                });

                self.bump();
                return Ok(());
            }

            Directive::Define => {
                if self.is_inactive() {
                    return Ok(());
                }

                // Skip leading whitespace.
                match self.token {
                    Some((Whitespace, _)) => self.bump(),
                    _ => (),
                }

                // Consume the macro name.
                let (name, name_span) = match self.try_eat_name() {
                    Some(x) => x,
                    None => {
                        return Err(
                            DiagBuilder2::fatal("expected macro name after \"`define\"").span(span)
                        );
                    }
                };
                let mut makro = Macro::new(name.clone(), name_span);

                // NOTE: No whitespace is allowed after the macro name such that
                // the preprocessor does not mistake the a in "`define FOO (a)"
                // for a macro argument.

                // Consume the macro arguments and parameters.
                match self.token {
                    Some((Symbol('('), _)) => {
                        self.bump();
                        loop {
                            // Skip whitespace.
                            match self.token {
                                Some((Whitespace, _)) => self.bump(),
                                Some((Symbol(')'), _)) => break,
                                _ => (),
                            }

                            // Consume the argument name.
                            let (name, name_span) = match self.try_eat_name() {
                                Some(x) => x,
                                _ => {
                                    return Err(DiagBuilder2::fatal(
                                        "expected macro argument name",
                                    )
                                    .span(span));
                                }
                            };
                            makro.args.push(MacroArg::new(name, name_span));
                            // TODO: Support default parameters.

                            // Skip whitespace and either consume the comma that
                            // follows or break out of the loop if a closing
                            // parenthesis is encountered.
                            match self.token {
                                Some((Whitespace, _)) => self.bump(),
                                _ => (),
                            }
                            match self.token {
                                Some((Symbol(','), _)) => self.bump(),
                                Some((Symbol(')'), _)) => break,
                                Some((_, sp)) => return Err(DiagBuilder2::fatal("expected , or ) after macro argument name").span(sp)),
                                None => return Err(DiagBuilder2::fatal("expected closing parenthesis at the end of the macro definition").span(span)),
                            }
                        }
                        self.bump();
                    }
                    _ => (),
                }

                // Skip whitespace between the macro parameters and definition.
                match self.token {
                    Some((Whitespace, _)) => self.bump(),
                    _ => (),
                }

                // Consume the macro definition up to the next newline not preceded
                // by a backslash, ignoring comments, whitespace and newlines.
                loop {
                    match self.token {
                        Some((Newline, _)) => {
                            self.bump();
                            break;
                        }
                        // Some((Whitespace, _)) => self.bump(),
                        // Some((Comment, _)) => self.bump(),
                        Some((Symbol('\\'), _)) => {
                            self.bump();
                            match self.token {
                                Some((Newline, _)) => self.bump(),
                                _ => (),
                            };
                        }
                        Some(x) => {
                            makro.body.push(x);
                            self.bump();
                        }
                        None => break,
                    }
                }

                self.macro_defs.insert(name, makro);
                return Ok(());
            }

            Directive::Undef => {
                if self.is_inactive() {
                    return Ok(());
                }

                // Skip leading whitespace.
                match self.token {
                    Some((Whitespace, _)) => self.bump(),
                    _ => (),
                }

                // Consume the macro name.
                let (name, _) = match self.try_eat_name() {
                    Some(x) => x,
                    None => {
                        return Err(
                            DiagBuilder2::fatal("expected macro name after \"`undef\"").span(span)
                        );
                    }
                };

                // Remove the macro definition.
                self.macro_defs.remove(&name);
                return Ok(());
            }

            Directive::Undefineall => {
                if self.is_inactive() {
                    return Ok(());
                }
                self.macro_defs.clear();
            }

            Directive::Ifdef | Directive::Ifndef | Directive::Elsif => {
                // Skip leading whitespace.
                match self.token {
                    Some((Whitespace, _)) => self.bump(),
                    _ => (),
                }

                // Consume the macro name.
                let name = match self.try_eat_name() {
                    Some((x, _)) => x,
                    _ => {
                        return Err(DiagBuilder2::fatal(format!(
                            "expected macro name after {}",
                            dir_name
                        ))
                        .span(span));
                    }
                };
                let exists = self.macro_defs.contains_key(&name);

                // Depending on the directive, modify the define conditional
                // stack.
                match dir {
                    Directive::Ifdef => self.defcond_stack.push(if self.is_inactive() {
                        Defcond::Done
                    } else if exists {
                        Defcond::Enabled
                    } else {
                        Defcond::Disabled
                    }),
                    Directive::Ifndef => self.defcond_stack.push(if self.is_inactive() {
                        Defcond::Done
                    } else if exists {
                        Defcond::Disabled
                    } else {
                        Defcond::Enabled
                    }),
                    Directive::Elsif => {
                        match self.defcond_stack.pop() {
                            Some(Defcond::Done) |
                            Some(Defcond::Enabled) => self.defcond_stack.push(Defcond::Done),
                            Some(Defcond::Disabled) => self.defcond_stack.push(
                                if self.is_inactive() {
                                    Defcond::Done
                                } else if exists {
                                    Defcond::Enabled
                                } else {
                                    Defcond::Disabled
                                }
                            ),
                            None => return Err(DiagBuilder2::fatal("found `elsif without any preceeding `ifdef, `ifndef, or `elsif directive").span(span))
                        };
                    }
                    _ => unreachable!(),
                }

                return Ok(());
            }

            Directive::Else => {
                match self.defcond_stack.pop() {
                    Some(Defcond::Disabled) => self.defcond_stack.push(Defcond::Enabled),
                    Some(Defcond::Enabled) | Some(Defcond::Done) => {
                        self.defcond_stack.push(Defcond::Done)
                    }
                    None => return Err(DiagBuilder2::fatal(
                        "found `else without any preceeding `ifdef, `ifndef, or `elsif directive",
                    )
                    .span(span)),
                }
                return Ok(());
            }

            Directive::Endif => {
                if self.defcond_stack.pop().is_none() {
                    return Err(DiagBuilder2::fatal("found `endif without any preceeding `ifdef, `ifndef, `else, or `elsif directive").span(span));
                }
                return Ok(());
            }

            // Perform macro substitution. If we're currently inside the
            // inactive region of a define conditional (i.e. disabled or done),
            // don't bother expanding the macro.
            Directive::Unknown => {
                if self.is_inactive() {
                    return Ok(());
                }
                if let Some(ref makro) = unsafe { &*(self as *const Preprocessor) }
                    .macro_defs
                    .get(dir_name)
                {
                    // Consume the macro parameters if the macro definition
                    // contains them.
                    let mut params = HashMap::<String, Vec<TokenAndSpan>>::new();
                    let mut args = makro.args.iter();
                    if !makro.args.is_empty() {
                        // // Skip whitespace.
                        // match self.token {
                        //  Some((Whitespace, _)) => self.bump(),
                        //  _ => ()
                        // }

                        // Consume the opening paranthesis.
                        match self.token {
                            Some((Symbol('('), _)) => self.bump(),
                            _ => {
                                return Err(DiagBuilder2::fatal(
                                    "expected macro parameters in parentheses '(...)'",
                                )
                                .span(span));
                            }
                        }

                        // Consume the macro parameters.
                        'outer: loop {
                            // // Skip whitespace and break out of the loop if the
                            // // closing parenthesis was encountered.
                            // match self.token {
                            //  Some((Whitespace, _)) => self.bump(),
                            //  _ => ()
                            // }
                            match self.token {
                                Some((Symbol(')'), _)) => break,
                                _ => (),
                            }

                            // Fetch the next argument.
                            let arg = match args.next() {
                                Some(arg) => arg,
                                None => {
                                    return Err(DiagBuilder2::fatal(
                                        "superfluous macro parameters",
                                    ));
                                }
                            };

                            // Consume the tokens that make up this argument.
                            // Take care that it is allowed to have parentheses
                            // as macro parameters, which requires bookkeeping
                            // of the parentheses nesting level. If a comma is
                            // encountered, we break out of the inner loop such
                            // that the next parameter will be read. If a
                            // closing parenthesis is encountered, we break out
                            // of the outer loop to finish parameter parsing.
                            let mut param_tokens = Vec::<TokenAndSpan>::new();
                            let mut nesting = 0;
                            loop {
                                match self.token {
                                    // Some((Whitespace, _)) => self.bump(),
                                    // Some((Newline, _)) => self.bump(),
                                    // Some((Comment, _)) => self.bump(),
                                    Some((Symbol(','), _)) if nesting == 0 => {
                                        self.bump();
                                        params.insert(arg.name.clone(), param_tokens);
                                        break;
                                    }
                                    Some((Symbol(')'), _)) if nesting == 0 => {
                                        params.insert(arg.name.clone(), param_tokens);
                                        break 'outer;
                                    }
                                    Some(x @ (Symbol('('), _)) => {
                                        param_tokens.push(x);
                                        self.bump();
                                        nesting += 1;
                                    }
                                    Some(x @ (Symbol(')'), _)) if nesting > 0 => {
                                        param_tokens.push(x);
                                        self.bump();
                                        nesting -= 1;
                                    }
                                    Some(x) => {
                                        param_tokens.push(x);
                                        self.bump();
                                    }
                                    None => {
                                        return Err(DiagBuilder2::fatal(
                                            "expected closing parenthesis after macro parameters",
                                        )
                                        .span(span));
                                    }
                                }
                            }
                        }
                        self.bump();
                    }

                    // Now we have a problem. All the tokens of the macro name
                    // have been parsed and we would like to continue by
                    // injecting the tokens of the macro body, such as to
                    // perform substitution. The token just after the macro use,
                    // e.g. the whitespace in "`foo ", is already in the buffer.
                    // However, we don't want this token to be the next, but
                    // rather have it follow after the macro expansion. To do
                    // this, we need to push the token onto the macro stack and
                    // then call `self.bump()` once the expansion has been added
                    // to the stack.
                    match self.token {
                        Some((x, sp)) => self.macro_stack.push((x, sp)),
                        None => (),
                    }

                    // Push the tokens of the macro onto the stack, potentially
                    // substituting any macro parameters as necessary.
                    if params.is_empty() {
                        self.macro_stack
                            .extend(makro.body.iter().rev().map(|&(tkn, sp)| (tkn, sp)));
                    } else {
                        let mut replacement = Vec::<TokenAndSpan>::new();
                        // TODO: Make this work for argument names that contain
                        // underscores.
                        for tkn in &makro.body {
                            match *tkn {
                                (Text, sp) => match params.get(&sp.extract()) {
                                    Some(substitute) => {
                                        replacement.extend(substitute);
                                    }
                                    None => replacement.push(*tkn),
                                },
                                x => replacement.push(x),
                            }
                        }
                        self.macro_stack
                            .extend(replacement.iter().rev().map(|&(tkn, sp)| (tkn, sp)));
                    }

                    self.bump();
                    return Ok(());
                }
            }

            // Ignore the "`timescale" directive for now.
            Directive::Timescale => {
                while let Some((tkn, _)) = self.token {
                    if tkn == Newline {
                        break;
                    }
                    self.bump();
                }
                return Ok(());
            }

            Directive::CurrentFile => {
                if !self.is_inactive() {
                    self.macro_stack.push((CatTokenKind::Text, span));
                }
                return Ok(());
            }

            Directive::CurrentLine => {
                if !self.is_inactive() {
                    self.macro_stack.push((CatTokenKind::Digits, span));
                }
                return Ok(());
            }

            Directive::Resetall => {
                if !self.is_inactive() {
                    self.dirs = Default::default();
                }
                return Ok(());
            }

            Directive::Celldefine => {
                if !self.is_inactive() {
                    self.dirs.celldefine = true;
                }
                return Ok(());
            }

            Directive::Endcelldefine => {
                if !self.is_inactive() {
                    self.dirs.celldefine = false;
                }
                return Ok(());
            }

            Directive::DefaultNettype => {
                if !self.is_inactive() {
                    // Skip leading whitespace.
                    match self.token {
                        Some((Whitespace, _)) => self.bump(),
                        _ => (),
                    }

                    // Parse the nettype.
                    let tkn = match self.token {
                        Some(tkn @ (Text, _)) => {
                            self.bump();
                            tkn
                        }
                        _ => {
                            return Err(DiagBuilder2::fatal(
                                "expected nettype after `default_nettype",
                            )
                            .span(span));
                        }
                    };

                    // Store the nettype in the directive set.
                    self.dirs.default_nettype = if tkn.1.extract() == "none" {
                        None
                    } else {
                        Some(tkn)
                    };
                    debug!(
                        "Set default_nettype to `{}`",
                        self.dirs
                            .default_nettype
                            .map(|(_, sp)| sp.extract())
                            .unwrap_or_else(|| "none".to_string())
                    );
                }
                return Ok(());
            }
        }

        return Err(
            DiagBuilder2::fatal(format!("unknown compiler directive '`{}'", dir_name)).span(span),
        );
    }

    fn open_include(&mut self, filename: &str, current_file: &str) -> Option<Source> {
        // println!("Resolving include '{}' from '{}'", filename, current_file);
        let first = [Path::new(current_file)
            .parent()
            .expect("current file path must have a valid parent")];
        let prefices = first.iter().chain(self.include_paths.iter());
        let sm = get_source_manager();
        for prefix in prefices {
            let mut buf = prefix.to_path_buf();
            buf.push(filename);
            // println!("  trying {}", buf.to_str().unwrap());
            let src = sm.open(buf.to_str().unwrap());
            if src.is_some() {
                return src;
            }
        }
        return None;
    }

    /// Check whether we are inside a disabled define conditional. That is,
    /// whether a preceeding `ifdef, `ifndef, `else, or `elsif directive have
    /// disabled the subsequent code.
    fn is_inactive(&self) -> bool {
        match self.defcond_stack.last() {
            Some(&Defcond::Enabled) | None => false,
            _ => true,
        }
    }

    fn try_eat_name(&mut self) -> Option<(String, Span)> {
        // Eat the first token of the name, which may either be a letter or an
        // underscore.
        let (mut name, mut span) = match self.token {
            Some((Text, sp)) | Some((Symbol('_'), sp)) => (sp.extract(), sp),
            _ => return None,
        };
        self.bump();

        // Eat the remaining tokens of the name, which may be letters, digits,
        // or underscores.
        loop {
            match self.token {
                Some((Text, sp)) | Some((Digits, sp)) | Some((Symbol('_'), sp)) => {
                    name.push_str(&sp.extract());
                    span.expand(sp);
                    self.bump();
                }
                _ => break,
            }
        }

        Some((name, span))
    }
}

impl<'a> Iterator for Preprocessor<'a> {
    type Item = DiagResult2<TokenAndSpan>;

    fn next(&mut self) -> Option<DiagResult2<TokenAndSpan>> {
        // In case this is the first call to next(), the token has not been
        // populated yet. In this case we need to artificially bump the lexer.
        if self.token.is_none() {
            self.bump();
        }
        loop {
            // This is the main loop of the lexer. Upon each iteration the next
            // token is inspected and the lexer decides whether to emit it or
            // not. If no token was emitted (e.g. because it was a preprocessor
            // directive or we're inside an inactive `ifdef block), the loop
            // continues with the next token.
            match self.token {
                Some((Symbol('`'), sp_backtick)) => {
                    self.bump(); // consume the backtick
                    if let Some((name, sp)) = self.try_eat_name() {
                        // We arrive here if the sequence a backtick
                        // followed by text was encountered. In this case we
                        // call upon the handle_directive function to
                        // perform the necessary actions.
                        let dir_span = Span::union(sp_backtick, sp);
                        match self.handle_directive(name, dir_span) {
                            Err(x) => return Some(Err(x)),
                            _ => (),
                        }
                        continue;
                    } else if let Some(tkn @ (Symbol('"'), _)) = self.token {
                        // emit the '"'
                        self.bump();
                        if !self.is_inactive() {
                            return Some(Ok(tkn));
                        }
                    } else if let Some(tkn @ (Symbol('\\'), _)) = self.token {
                        // emit the '\'
                        self.bump();
                        if !self.is_inactive() {
                            return Some(Ok(tkn));
                        }
                    } else if let Some((Symbol('`'), _)) = self.token {
                        self.bump(); // consume the second backtick and ignore
                    } else {
                        return Some(Err(DiagBuilder2::fatal(
                            "expected compiler directive after '`', or '``', '`\"', or '`\\'",
                        )
                        .span(sp_backtick)));
                    }
                }
                _ => {
                    // All tokens other than preprocessor directives are
                    // emitted, unless we're currently inside a disabled define
                    // conditional.
                    if self.is_inactive() {
                        self.bump();
                    } else {
                        let tkn = self.token.map(|x| Ok(x));
                        self.bump();
                        return tkn;
                    }
                }
            }
        }
    }
}

struct Stream<'a> {
    source: Source,
    iter: Cat<'a>,
}

/// The different compiler directives recognized by the preprocessor.
#[derive(Debug, Clone, Copy)]
enum Directive {
    Include,
    Define,
    Undef,
    Undefineall,
    Ifdef,
    Ifndef,
    Else,
    Elsif,
    Endif,
    Timescale,
    CurrentFile,
    CurrentLine,
    Resetall,
    Celldefine,
    Endcelldefine,
    DefaultNettype,
    Unknown,
}

impl fmt::Display for Directive {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Directive::Include => write!(f, "`include"),
            Directive::Define => write!(f, "`define"),
            Directive::Undef => write!(f, "`undef"),
            Directive::Undefineall => write!(f, "`undefineall"),
            Directive::Ifdef => write!(f, "`ifdef"),
            Directive::Ifndef => write!(f, "`ifndef"),
            Directive::Else => write!(f, "`else"),
            Directive::Elsif => write!(f, "`elsif"),
            Directive::Endif => write!(f, "`endif"),
            Directive::Timescale => write!(f, "`timescale"),
            Directive::CurrentFile => write!(f, "`__FILE__"),
            Directive::CurrentLine => write!(f, "`__LINE__"),
            Directive::Resetall => write!(f, "`resetall"),
            Directive::Celldefine => write!(f, "`celldefine"),
            Directive::Endcelldefine => write!(f, "`endcelldefine"),
            Directive::DefaultNettype => write!(f, "`default_nettype"),
            Directive::Unknown => write!(f, "unknown"),
        }
    }
}

thread_local!(static DIRECTIVES_TABLE: HashMap<&'static str, Directive> = {
    let mut table = HashMap::new();
    table.insert("include", Directive::Include);
    table.insert("define", Directive::Define);
    table.insert("undef", Directive::Undef);
    table.insert("undefineall", Directive::Undefineall);
    table.insert("ifdef", Directive::Ifdef);
    table.insert("ifndef", Directive::Ifndef);
    table.insert("else", Directive::Else);
    table.insert("elsif", Directive::Elsif);
    table.insert("endif", Directive::Endif);
    table.insert("__FILE__", Directive::CurrentFile);
    table.insert("__LINE__", Directive::CurrentLine);
    table.insert("resetall", Directive::Resetall);
    table.insert("celldefine", Directive::Celldefine);
    table.insert("endcelldefine", Directive::Endcelldefine);
    table.insert("default_nettype", Directive::DefaultNettype);
    table.insert("timescale", Directive::Timescale);
    table
});

#[derive(Debug)]
struct Macro {
    name: String,
    span: Span,
    args: Vec<MacroArg>,
    body: Vec<TokenAndSpan>,
}

impl Macro {
    fn new(name: String, span: Span) -> Macro {
        Macro {
            name: name,
            span: span,
            args: Vec::new(),
            body: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct MacroArg {
    name: String,
    span: Span,
}

impl MacroArg {
    fn new(name: String, span: Span) -> MacroArg {
        MacroArg {
            name: name,
            span: span,
        }
    }
}

enum Defcond {
    Done,
    Enabled,
    Disabled,
}

#[derive(Default)]
struct Directives {
    celldefine: bool,
    default_nettype: Option<TokenAndSpan>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preproc(input: &str) -> Preprocessor {
        use std::cell::Cell;
        thread_local!(static INDEX: Cell<usize> = Cell::new(0));
        let sm = get_source_manager();
        let idx = INDEX.with(|i| {
            let v = i.get();
            i.set(v + 1);
            v
        });
        let source = sm.add(&format!("test_{}.sv", idx), input);
        Preprocessor::new(source, &[], &[])
    }

    fn check_str(input: &str, expected: &str) {
        let pp = preproc(input);
        let actual: String = pp.map(|x| x.unwrap().1.extract()).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn include() {
        let sm = get_source_manager();
        sm.add("other.sv", "bar\n");
        sm.add("test.sv", "foo\n`include \"other.sv\"\nbaz");
        let pp = Preprocessor::new(sm.open("test.sv").unwrap(), &[], &[]);
        let actual: Vec<_> = pp.map(|x| x.unwrap().0).collect();
        assert_eq!(actual, &[Text, Newline, Text, Newline, Newline, Text,]);
    }

    #[test]
    fn include_and_define() {
        let sm = get_source_manager();
        sm.add("other.sv", "/* World */\n`define foo 42\nbar");
        sm.add(
            "test.sv",
            "// Hello\n`include \"other.sv\"\n`foo something\n",
        );
        let pp = Preprocessor::new(sm.open("test.sv").unwrap(), &[], &[]);
        let actual: String = pp
            .map(|x| {
                let x = x.unwrap();
                println!("{:?}", x);
                x.1.extract()
            })
            .collect();
        assert_eq!(actual, "// Hello\n/* World */\nbar\n42 something\n");
    }

    #[test]
    #[should_panic(expected = "unknown compiler directive")]
    fn conditional_define() {
        let sm = get_source_manager();
        let source = sm.add("test.sv", "`ifdef FOO\n`define BAR\n`endif\n`BAR");
        let mut pp = Preprocessor::new(source, &[], &[]);
        while let Some(tkn) = pp.next() {
            tkn.unwrap();
        }
    }

    #[test]
    fn macro_args() {
        check_str(
            "`define foo(x,y) {x + y _bar}\n`foo(12, foo)\n",
            "{12 +  foo _bar}\n",
        );
    }

    /// Verify that macros that take no arguments but have parantheses around
    /// their body parse properly.
    #[test]
    fn macro_noargs_parentheses() {
        check_str(
            "`define FOO 4\n`define BAR (`FOO+$clog2(2))\n`BAR",
            "(4+$clog2(2))",
        );
    }

    #[test]
    fn macro_name_with_digits_and_underscores() {
        check_str("`define AXI_BUS21_SV 42\n`AXI_BUS21_SV", "42");
    }
}
