//! "Now playing" via the Global System Media Transport Controls (the same source as
//! the Windows media flyout). Runs on its own thread because the WinRT calls block.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

#[derive(Clone, Copy)]
pub enum Command {
    Prev,
    Toggle,
    Next,
}

#[derive(Clone, Default, PartialEq)]
pub struct NowPlaying {
    pub active: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
}

/// Starts the media thread. `on_change` is called (on that thread) whenever the track or state changes.
pub fn spawn(on_change: impl Fn(NowPlaying) + Send + 'static) -> Sender<Command> {
    let (tx, rx) = channel();
    std::thread::spawn(move || run(rx, on_change));
    tx
}

fn run(rx: Receiver<Command>, on_change: impl Fn(NowPlaying)) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let manager = loop {
        match Manager::RequestAsync().and_then(|op| op.join()) {
            Ok(m) => break m,
            Err(_) => std::thread::sleep(Duration::from_secs(5)),
        }
    };
    let mut last = NowPlaying::default();
    loop {
        match rx.recv_timeout(Duration::from_millis(1000)) {
            Ok(cmd) => {
                let _ = execute(&manager, cmd);
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = read(&manager).unwrap_or_default();
        if now != last {
            on_change(now.clone());
            last = now;
        }
    }
}

fn read(manager: &Manager) -> windows::core::Result<NowPlaying> {
    let session = manager.GetCurrentSession()?;
    let props = session.TryGetMediaPropertiesAsync()?.join()?;
    let status = session.GetPlaybackInfo()?.PlaybackStatus()?;
    let title = props.Title()?.to_string();
    Ok(NowPlaying {
        active: !title.is_empty(),
        playing: status == Status::Playing,
        title,
        artist: props.Artist()?.to_string(),
    })
}

fn execute(manager: &Manager, cmd: Command) -> windows::core::Result<()> {
    let session = manager.GetCurrentSession()?;
    let op = match cmd {
        Command::Prev => session.TrySkipPreviousAsync()?,
        Command::Toggle => session.TryTogglePlayPauseAsync()?,
        Command::Next => session.TrySkipNextAsync()?,
    };
    op.join()?;
    Ok(())
}
