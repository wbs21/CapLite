use crossbeam_channel::{bounded, Sender, Receiver};
use std::thread;
use std::io::{self, Write};

#[derive(Debug)]
pub enum LogMsg {
    Info(String),
    Warn(String),
    Error(String),
    Exit,
}

pub struct AsyncLogger {
    tx: Sender<LogMsg>,
}

static GLOBAL_LOGGER: once_cell::sync::Lazy<AsyncLogger> = once_cell::sync::Lazy::new(|| {
    let (tx, rx) = bounded(512);
    thread::spawn(move || logger_thread(rx));
    AsyncLogger { tx }
});

fn logger_thread(rx: Receiver<LogMsg>) {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    loop {
        match rx.recv() {
            Ok(LogMsg::Info(s)) => writeln!(handle, "[INFO] {}", s).ok(),
            Ok(LogMsg::Warn(s)) => writeln!(handle, "[WARN] {}", s).ok(),
            Ok(LogMsg::Error(s)) => writeln!(handle, "[ERROR] {}", s).ok(),
            Ok(LogMsg::Exit) => break,
            Err(_) => break,
        };
    }
}

pub fn log_info(s: impl Into<String>) {
    let _ = GLOBAL_LOGGER.tx.try_send(LogMsg::Info(s.into()));
}
pub fn log_warn(s: impl Into<String>) {
    let _ = GLOBAL_LOGGER.tx.try_send(LogMsg::Warn(s.into()));
}
pub fn log_error(s: impl Into<String>) {
    let _ = GLOBAL_LOGGER.tx.try_send(LogMsg::Error(s.into()));
}
pub fn log_close() {
    let _ = GLOBAL_LOGGER.tx.send(LogMsg::Exit);
}
