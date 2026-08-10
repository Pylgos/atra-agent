use std::{
    env,
    io::{self, Write},
};

const TITLE: &str = "Atra";

#[derive(Clone, Copy)]
enum Protocol {
    Osc9,
    Osc99,
}

pub(crate) fn send(message: &str) -> io::Result<()> {
    let message = preview(message);
    let protocol = if env::var_os("KITTY_WINDOW_ID").is_some() {
        Protocol::Osc99
    } else {
        Protocol::Osc9
    };
    let mut stdout = io::stdout().lock();
    write_notification(&mut stdout, protocol, &message)?;
    stdout.flush()
}

fn preview(message: &str) -> String {
    let mut preview = String::new();
    let mut pending_space = false;
    for character in message.chars() {
        if character.is_whitespace() {
            pending_space = !preview.is_empty();
            continue;
        }
        if character.is_control() {
            continue;
        }
        if pending_space {
            if preview.chars().count() == 200 {
                break;
            }
            preview.push(' ');
            pending_space = false;
        }
        if preview.chars().count() == 200 {
            break;
        }
        preview.push(character);
    }
    if preview.is_empty() {
        "Turn completed".to_owned()
    } else {
        preview
    }
}

fn write_notification(
    writer: &mut impl Write,
    protocol: Protocol,
    message: &str,
) -> io::Result<()> {
    match protocol {
        Protocol::Osc9 => write!(writer, "\x1b]9;{TITLE}: {message}\x1b\\"),
        Protocol::Osc99 => write!(writer, "\x1b]99;;{TITLE}: {message}\x1b\\"),
    }
}
