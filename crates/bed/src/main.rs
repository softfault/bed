#![forbid(unsafe_code)]

use anyhow::Result;
use bed_core::Editor;
use bed_terminal::Terminal;
use bed_tui::App;
use std::{env, ffi::OsString, path::PathBuf};

fn main() -> Result<()> {
    let paths = parse_paths(env::args_os().skip(1))?;
    let mut app = App::new(Editor::open_paths(paths)?);
    let mut terminal = Terminal::new()?;

    while !app.should_quit() {
        let frame = app.render(terminal.size()?);
        terminal.draw(&frame)?;
        app.handle_key(terminal.read_key()?)?;
    }
    Ok(())
}

fn parse_paths(arguments: impl IntoIterator<Item = OsString>) -> Result<Vec<PathBuf>> {
    let paths: Vec<_> = arguments.into_iter().map(PathBuf::from).collect();
    anyhow::ensure!(!paths.is_empty(), "usage: bed <file> [file ...]");
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::parse_paths;
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn accepts_one_or_more_startup_paths() {
        assert_eq!(
            parse_paths([OsString::from("one"), OsString::from("two")]).unwrap(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert!(parse_paths(Vec::<OsString>::new()).is_err());
    }
}
