#![forbid(unsafe_code)]

use anyhow::Result;
use bed_core::{Document, Editor};
use bed_terminal::{Terminal, TerminalEvent};
use bed_tui::App;
use std::{env, ffi::OsString, path::PathBuf, time::Duration};

fn main() -> Result<()> {
    let paths = parse_paths(env::args_os().skip(1))?;
    let mut app = open_app(paths)?;
    let mut terminal = Terminal::new()?;
    let mut size = terminal.size()?;
    let events = terminal.events()?;
    let mut redraw = true;

    while !app.should_quit() {
        if redraw {
            let frame = app.render(size);
            terminal.draw(&frame)?;
            redraw = false;
        }
        let batch = events.next_batch(Duration::from_millis(16))?;
        for event in batch {
            if app.should_quit() {
                break;
            }
            match event {
                TerminalEvent::Key(key) => redraw |= app.handle_key(key)?,
                TerminalEvent::Mouse(mouse) => redraw |= app.handle_mouse(mouse),
                TerminalEvent::Resize(resized) => {
                    size = resized;
                    app.handle_resize(resized);
                    redraw = true;
                }
            }
        }
        redraw |= app.poll_terminals()?;
    }
    Ok(())
}

fn parse_paths(arguments: impl IntoIterator<Item = OsString>) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<_> = arguments.into_iter().map(PathBuf::from).collect();
    if paths.is_empty() {
        paths.push(PathBuf::from("."));
    }
    Ok(paths)
}

fn open_app(mut paths: Vec<PathBuf>) -> Result<App> {
    anyhow::ensure!(!paths.is_empty(), "at least one startup path is required");
    if !paths[0].is_dir() {
        return Ok(App::new(Editor::open_paths(paths)?));
    }

    let directory = paths.remove(0);
    let editor = if paths.is_empty() {
        Editor::new(Document::scratch())
    } else {
        Editor::open_paths(paths)?
    };
    Ok(App::for_directory(editor, directory))
}

#[cfg(test)]
mod tests {
    use super::{open_app, parse_paths};
    use bed_tui::Mode;
    use std::{ffi::OsString, path::PathBuf};

    #[test]
    fn accepts_one_or_more_startup_paths() {
        assert_eq!(
            parse_paths([OsString::from("one"), OsString::from("two")]).unwrap(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(
            parse_paths(Vec::<OsString>::new()).unwrap(),
            [PathBuf::from(".")]
        );
    }

    #[test]
    fn opens_a_directory_in_the_file_tree() {
        let app = open_app(vec![PathBuf::from(".")]).unwrap();

        assert_eq!(app.mode(), Mode::Tree);
        assert_eq!(app.editor().document().path(), PathBuf::from("[No Name]"));
        assert!(!app.editor().document().is_file_backed());
    }
}
