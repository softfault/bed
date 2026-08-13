use bed_vt100::{Attributes, Color, Screen, TerminalEmulator};

struct Transcript {
    name: &'static str,
    rows: usize,
    columns: usize,
    bytes: &'static [u8],
    expected: &'static [&'static str],
    title: &'static str,
    alternate: bool,
}

const TRANSCRIPTS: &[Transcript] = &[
    Transcript {
        name: "colored shell prompt",
        rows: 4,
        columns: 32,
        bytes: b"\x1b]2;bed shell\x07\x1b[32muser@host\x1b[0m:\x1b[34m~/src\x1b[0m$ printf ok\r\nok\r\n",
        expected: &["user@host:~/src$ printf ok", "ok", "", ""],
        title: "bed shell",
        alternate: false,
    },
    Transcript {
        name: "carriage-return progress repaint",
        rows: 3,
        columns: 20,
        bytes: b"download 10%\r\x1b[Kdownload 50%\r\x1b[Kdownload 100%\r\ncomplete",
        expected: &["download 100%", "complete", ""],
        title: "",
        alternate: false,
    },
    Transcript {
        name: "alternate-screen application",
        rows: 4,
        columns: 18,
        bytes: b"shell before\r\n\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H\x1b[7m STATUS \x1b[0m\r\nitem one\r\nitem two\x1b[?2004h",
        expected: &[" STATUS", "item one", "item two", ""],
        title: "",
        alternate: true,
    },
];

#[test]
fn replays_common_terminal_transcripts_at_every_input_boundary() {
    for fixture in TRANSCRIPTS {
        let expected = replay(fixture, &[fixture.bytes]);
        assert_fixture(fixture, &expected);

        for split in 0..=fixture.bytes.len() {
            let actual = replay(fixture, &[&fixture.bytes[..split], &fixture.bytes[split..]]);
            assert_eq!(
                snapshot(actual.screen()),
                snapshot(expected.screen()),
                "{} differed at split {split}",
                fixture.name
            );
            assert_eq!(actual.title(), expected.title(), "{} title", fixture.name);
            assert_eq!(
                actual.alternate_screen_active(),
                expected.alternate_screen_active(),
                "{} screen mode",
                fixture.name
            );
        }
    }
}

#[test]
fn preserves_attributes_in_transcript_cells() {
    let shell = replay(&TRANSCRIPTS[0], &[TRANSCRIPTS[0].bytes]);
    assert_eq!(
        shell.screen().cell(0, 0).unwrap().attributes().foreground,
        Color::Indexed(2)
    );
    assert_eq!(
        shell.screen().cell(0, 10).unwrap().attributes().foreground,
        Color::Indexed(4)
    );

    let application = replay(&TRANSCRIPTS[2], &[TRANSCRIPTS[2].bytes]);
    assert!(
        application
            .screen()
            .cell(0, 0)
            .unwrap()
            .attributes()
            .inverse
    );
    assert!(!application.screen().cursor().visible);
    assert!(application.modes().bracketed_paste);
    assert_eq!(
        application.primary_screen().row(0).unwrap().text(),
        "shell before"
    );
    assert!(application.primary_screen().scrollback().is_empty());
}

fn replay(fixture: &Transcript, chunks: &[&[u8]]) -> TerminalEmulator {
    let mut terminal = TerminalEmulator::new(fixture.rows, fixture.columns, 100);
    for chunk in chunks {
        terminal.process(chunk);
    }
    terminal
}

fn assert_fixture(fixture: &Transcript, terminal: &TerminalEmulator) {
    let rows: Vec<_> = terminal
        .screen()
        .rows()
        .iter()
        .map(|row| row.text())
        .collect();
    assert_eq!(rows, fixture.expected, "{} contents", fixture.name);
    assert_eq!(terminal.title(), fixture.title, "{} title", fixture.name);
    assert_eq!(
        terminal.alternate_screen_active(),
        fixture.alternate,
        "{} screen mode",
        fixture.name
    );
}

fn snapshot(screen: &Screen) -> Vec<(String, Attributes, bool)> {
    screen
        .rows()
        .iter()
        .flat_map(|row| {
            row.cells().iter().map(|cell| {
                (
                    cell.contents().to_owned(),
                    cell.attributes(),
                    cell.is_continuation(),
                )
            })
        })
        .collect()
}
