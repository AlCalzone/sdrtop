// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use crossterm::event::{self, Event, KeyEvent};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use crate::hardware::DeviceOption;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceOptionRequest {
    pub request_id: u64,
    pub id: String,
    pub label: String,
    pub choice: String,
}

#[derive(Debug)]
pub struct DeviceOptionCompletion {
    pub request: DeviceOptionRequest,
    pub result: Result<Vec<DeviceOption>, String>,
}

pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    DeviceOptionComplete(DeviceOptionCompletion),
}

pub struct EventStream {
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
}

impl EventStream {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let event_tx = tx.clone();
        thread::spawn(move || loop {
            if event::poll(tick_rate).unwrap_or(false) {
                match event::read() {
                    Ok(Event::Key(key)) => {
                        if event_tx.send(AppEvent::Key(key)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(..))
                        // Trigger an immediate redraw so preferred_height re-runs with the new width.
                        if event_tx.send(AppEvent::Tick).is_err() => {
                            break;
                        }
                    _ => {}
                }
            } else if event_tx.send(AppEvent::Tick).is_err() {
                break;
            }
        });
        Self { tx, rx }
    }

    pub fn recv(&self) -> AppEvent {
        self.rx.recv().unwrap_or(AppEvent::Tick)
    }

    pub fn sender(&self) -> Sender<AppEvent> {
        self.tx.clone()
    }
}
