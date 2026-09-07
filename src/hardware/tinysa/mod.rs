// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

mod discovery;
mod protocol;

use std::io::ErrorKind;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};

use crate::hardware::{
    AcquisitionKind, DeliveryModel, DeviceCapabilities, DeviceInfo, DeviceListing, GainModel,
    LevelUnit, PowerTrace, RxContext, SampleFormat, SampleGeometry, SdrDevice, SoftwareStack,
};

use super::traits::RateSet;
use protocol::{Identity, Model, PROMPT};

const MIN_FREQUENCY_HZ: u64 = 100_000;
const DEFAULT_FREQUENCY_HZ: u64 = 100_000_000;
const DEFAULT_SPAN_HZ: u64 = 10_000_000;
const DEFAULT_POINTS: u32 = 450;
const READ_TIMEOUT: Duration = Duration::from_millis(50);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const DRAIN_QUIET: Duration = Duration::from_millis(200);
const MAX_RESPONSE_BYTES: usize = 128 * 1024;

type UnitReply = Sender<anyhow::Result<()>>;

#[derive(Clone, Copy)]
struct ScanSettings {
    points: u32,
    rbw_khz: Option<f64>,
    spur: SpurMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpurMode {
    On,
    Auto,
}

pub fn list() -> Vec<DeviceListing> {
    discovery::list()
}

pub struct TinySaDevice {
    caps: DeviceCapabilities,
    info: DeviceInfo,
    notes: Vec<String>,
    command_tx: Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl TinySaDevice {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let port = serialport::new(path.to_string_lossy(), 115_200)
            .data_bits(DataBits::Eight)
            .stop_bits(StopBits::One)
            .parity(Parity::None)
            .flow_control(FlowControl::None)
            .timeout(READ_TIMEOUT)
            .open()
            .with_context(|| format!("failed to open tinySA at {}", path.display()))?;
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (init_tx, init_rx) = bounded(1);
        let worker = thread::Builder::new()
            .name("tinysa-serial".to_string())
            .spawn(move || worker_entry(port, command_rx, init_tx))
            .context("failed to start tinySA serial worker")?;
        let initialized = match init_rx.recv() {
            Ok(Ok(initialized)) => initialized,
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error);
            }
            Err(_) => {
                let _ = worker.join();
                bail!("tinySA serial worker stopped during initialization");
            }
        };
        let caps = capabilities(initialized.identity.model);
        let notes = initialized
            .identity
            .hardware
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let info = DeviceInfo {
            board_name: initialized.identity.board.clone(),
            serial: path.display().to_string(),
            stack: Some(SoftwareStack {
                label: "tinysa fw ",
                value: Arc::from(initialized.identity.firmware.as_str()),
            }),
            ..DeviceInfo::default()
        };
        Ok(Self {
            caps,
            info,
            notes,
            command_tx,
            worker: Mutex::new(Some(worker)),
        })
    }

    fn request<T>(
        &self,
        make_command: impl FnOnce(Sender<anyhow::Result<T>>) -> Command,
    ) -> anyhow::Result<T> {
        let (reply_tx, reply_rx) = bounded(1);
        self.command_tx
            .send(make_command(reply_tx))
            .map_err(|_| anyhow!("tinySA serial worker is not running"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("tinySA serial worker stopped without replying"))?
    }
}

impl SdrDevice for TinySaDevice {
    fn capabilities(&self) -> &DeviceCapabilities {
        &self.caps
    }

    fn info(&self) -> DeviceInfo {
        self.info.clone()
    }

    fn start_rx(&self, ctx: Arc<RxContext>) -> anyhow::Result<()> {
        self.request(|reply| Command::Start(ctx, reply))
    }

    fn stop_rx(&self) -> anyhow::Result<()> {
        self.request(Command::Stop)
    }

    fn is_streaming(&self) -> bool {
        self.request(Command::IsStreaming).unwrap_or(false)
    }

    fn set_frequency(&self, hz: u64) -> anyhow::Result<()> {
        self.request(|reply| Command::SetFrequency(hz, reply))
    }

    fn set_sample_rate(&self, hz: f64) -> anyhow::Result<RateSet> {
        self.request(|reply| Command::SetSpan(hz, reply))
    }

    fn set_lna_gain(&self, _db: u32) -> anyhow::Result<()> {
        self.request(Command::NoOp)
    }

    fn open_notes(&self) -> &[String] {
        &self.notes
    }
}

impl Drop for TinySaDevice {
    fn drop(&mut self) {
        let (reply_tx, reply_rx) = bounded(1);
        let _ = self.command_tx.send(Command::Shutdown(reply_tx));
        let _ = reply_rx.recv_timeout(Duration::from_secs(2));
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = worker.join();
        }
    }
}

enum Command {
    Start(Arc<RxContext>, UnitReply),
    Stop(UnitReply),
    IsStreaming(Sender<anyhow::Result<bool>>),
    SetFrequency(u64, UnitReply),
    SetSpan(f64, Sender<anyhow::Result<RateSet>>),
    NoOp(UnitReply),
    Shutdown(UnitReply),
}

struct Initialized {
    identity: Identity,
}

struct Worker {
    port: Box<dyn SerialPort>,
    command_rx: Receiver<Command>,
    identity: Identity,
    settings: ScanSettings,
    center_hz: u64,
    span_hz: u64,
    rx_context: Option<Arc<RxContext>>,
    active_path: Option<RfPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RfPath {
    Lower,
    Upper,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    start_hz: u64,
    stop_hz: u64,
    points: u32,
    path: RfPath,
}

enum ScanResult {
    Complete {
        frequencies_hz: Vec<u64>,
        levels_dbm: Vec<f32>,
        effective_center_hz: u64,
        effective_span_hz: u64,
    },
    Interrupted(Command, anyhow::Result<()>),
}

enum ByteEvent {
    Byte(u8),
    Command(Command),
}

fn worker_entry(
    mut port: Box<dyn SerialPort>,
    command_rx: Receiver<Command>,
    init_tx: Sender<anyhow::Result<Initialized>>,
) {
    let (identity, settings) = match initialize(&mut *port) {
        Ok(value) => value,
        Err(error) => {
            let _ = init_tx.send(Err(error));
            return;
        }
    };
    if init_tx
        .send(Ok(Initialized {
            identity: identity.clone(),
        }))
        .is_err()
    {
        return;
    }
    Worker {
        port,
        command_rx,
        identity,
        settings,
        center_hz: DEFAULT_FREQUENCY_HZ,
        span_hz: DEFAULT_SPAN_HZ,
        rx_context: None,
        active_path: None,
    }
    .run();
}

impl Worker {
    fn run(mut self) {
        loop {
            if self.rx_context.is_none() {
                match self.command_rx.recv() {
                    Ok(command) => {
                        if self.handle_command(command) {
                            continue;
                        }
                        return;
                    }
                    Err(_) => return,
                }
            }

            match self.scan_once() {
                Ok(ScanResult::Complete {
                    frequencies_hz,
                    levels_dbm,
                    effective_center_hz,
                    effective_span_hz,
                }) => {
                    self.center_hz = effective_center_hz;
                    self.span_hz = effective_span_hz;
                    if let Some(context) = &self.rx_context {
                        let published = context
                            .power_tx
                            .try_send(PowerTrace {
                                frequencies_hz,
                                levels_dbm,
                                rbw_hz: self
                                    .settings
                                    .rbw_khz
                                    .map(|khz| (khz * 1_000.0).round() as u32),
                            })
                            .is_ok();
                        if published {
                            let mut metrics = context
                                .metrics
                                .lock()
                                .unwrap_or_else(|error| error.into_inner());
                            metrics.radio.frequency = effective_center_hz;
                            metrics.radio.config_sample_rate = effective_span_hz as f64;
                        }
                    }
                    if !self.drain_commands() {
                        return;
                    }
                }
                Ok(ScanResult::Interrupted(command, abort_result)) => {
                    if let Err(error) = abort_result {
                        let shutting_down = matches!(&command, Command::Shutdown(_));
                        let message = error.to_string();
                        self.stop_acquisition(&message);
                        reject_command(command, anyhow!(message));
                        if shutting_down {
                            return;
                        }
                        continue;
                    }
                    if !self.handle_command(command) || !self.drain_commands() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = abort_active_scan(&mut *self.port);
                    self.stop_acquisition(&error.to_string());
                }
            }
        }
    }

    fn drain_commands(&mut self) -> bool {
        while let Ok(command) = self.command_rx.try_recv() {
            if !self.handle_command(command) {
                return false;
            }
        }
        true
    }

    fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Start(context, reply) => {
                let result = if self.rx_context.is_some() {
                    Err(anyhow!("tinySA acquisition is already running"))
                } else {
                    self.rx_context = Some(context);
                    Ok(())
                };
                let _ = reply.send(result);
            }
            Command::Stop(reply) => {
                self.rx_context = None;
                let _ = reply.send(Ok(()));
            }
            Command::IsStreaming(reply) => {
                let _ = reply.send(Ok(self.rx_context.is_some()));
            }
            Command::SetFrequency(hz, reply) => {
                self.center_hz = hz.clamp(MIN_FREQUENCY_HZ, self.identity.model.maximum_hz());
                let _ = reply.send(Ok(()));
            }
            Command::SetSpan(hz, reply) => {
                let result = if !hz.is_finite() || hz <= 0.0 {
                    Err(anyhow!("tinySA span must be a positive finite value"))
                } else {
                    let maximum = self.identity.model.maximum_hz() - MIN_FREQUENCY_HZ;
                    self.span_hz = (hz.round() as u64).clamp(1, maximum);
                    Ok(RateSet::new(
                        hz,
                        Some(self.span_hz as f64),
                        self.settings
                            .rbw_khz
                            .map(|khz| (khz * 1_000.0).round() as u32)
                            .unwrap_or(0),
                    ))
                };
                let _ = reply.send(result);
            }
            Command::NoOp(reply) => {
                let _ = reply.send(Ok(()));
            }
            Command::Shutdown(reply) => {
                self.rx_context = None;
                let _ = reply.send(Ok(()));
                return false;
            }
        }
        true
    }

    fn stop_acquisition(&mut self, message: &str) {
        if let Some(context) = self.rx_context.take() {
            let mut metrics = context
                .metrics
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            metrics.radio.hw_streaming = false;
            metrics.push_log(format!("tinySA scan error: {message}"));
        }
    }

    fn scan_once(&mut self) -> anyhow::Result<ScanResult> {
        let (start_hz, stop_hz) = centered_window(
            self.center_hz,
            self.span_hz,
            MIN_FREQUENCY_HZ,
            self.identity.model.maximum_hz(),
        );
        let points = self.settings.points;
        let mut frequencies_hz = Vec::with_capacity(points as usize);
        let mut levels_dbm = Vec::with_capacity(points as usize);
        for segment in scan_segments(self.identity.model, start_hz, stop_hz, points) {
            if let Some(command) = poll_scan_command(&self.command_rx)? {
                return Ok(ScanResult::Interrupted(command, Ok(())));
            }
            self.select_path(segment.path)?;
            let inactivity_timeout =
                scan_inactivity_timeout(segment, self.identity.model, self.settings)?;
            match scan_segment(
                &mut *self.port,
                &self.command_rx,
                segment,
                self.identity.zero_dbm,
                inactivity_timeout,
            )? {
                ScanResult::Complete {
                    frequencies_hz: segment_frequencies,
                    levels_dbm: segment_levels,
                    ..
                } => {
                    frequencies_hz.extend(segment_frequencies);
                    levels_dbm.extend(segment_levels);
                }
                interrupted => return Ok(interrupted),
            }
        }
        Ok(ScanResult::Complete {
            frequencies_hz: uniform_display_frequencies(&frequencies_hz)?,
            levels_dbm,
            effective_center_hz: start_hz + (stop_hz - start_hz) / 2,
            effective_span_hz: stop_hz - start_hz,
        })
    }

    fn select_path(&mut self, path: RfPath) -> anyhow::Result<()> {
        if self.active_path == Some(path) {
            return Ok(());
        }
        let command = match (self.identity.model.is_ultra(), path) {
            (false, RfPath::Lower) => "mode low input",
            (false, RfPath::Upper) => "mode high input",
            (true, RfPath::Lower) => "ultra off",
            (true, RfPath::Upper) => "ultra on",
        };
        send_text_command(&mut *self.port, command)?;
        if !self.identity.model.is_ultra() {
            send_text_command(&mut *self.port, "abort on")?;
            apply_scan_settings(&mut *self.port, self.identity.model)?;
        }
        self.active_path = Some(path);
        Ok(())
    }
}

fn initialize(port: &mut dyn SerialPort) -> anyhow::Result<(Identity, ScanSettings)> {
    best_effort_abort(port);
    drain_startup(port)?;
    let mut last_error = None;
    let mut version = None;
    for _ in 0..3 {
        match send_text_command(port, "version") {
            Ok(response)
                if String::from_utf8_lossy(&response)
                    .to_ascii_lowercase()
                    .contains("tinysa") =>
            {
                version = Some(response);
                break;
            }
            Ok(_) => last_error = Some(anyhow!("serial device did not identify as a tinySA")),
            Err(error) => last_error = Some(error),
        }
        let _ = drain_startup(port);
    }
    let version = version
        .ok_or_else(|| last_error.unwrap_or_else(|| anyhow!("tinySA version probe failed")))?;
    send_text_command(port, "output off")?;
    let info = send_text_command(port, "info")?;
    let help = send_text_command(port, "help")?;
    let zero = send_text_command(port, "zero")?;
    let identity = protocol::parse_identity(&version, &info, &help, &zero)?;
    send_text_command(port, "abort on")?;
    send_text_command(port, input_mode_command(identity.model))?;
    let (settings, _) = startup_settings(identity.model);
    apply_scan_settings(port, identity.model)?;
    Ok((identity, settings))
}

fn apply_scan_settings(port: &mut dyn SerialPort, model: Model) -> anyhow::Result<()> {
    for command in startup_settings(model).1 {
        send_text_command(port, command)?;
    }
    Ok(())
}

fn best_effort_abort(port: &mut dyn SerialPort) {
    let _ = port.write_all(b"abort\r");
    let _ = port.flush();
}

fn input_mode_command(model: Model) -> &'static str {
    if model.is_ultra() {
        "mode input"
    } else {
        "mode low input"
    }
}

fn send_text_command(port: &mut dyn SerialPort, command: &str) -> anyhow::Result<Vec<u8>> {
    port.write_all(command.as_bytes())
        .with_context(|| format!("failed to write tinySA {command} command"))?;
    port.write_all(b"\r")
        .with_context(|| format!("failed to terminate tinySA {command} command"))?;
    port.flush()
        .with_context(|| format!("failed to flush tinySA {command} command"))?;
    let frame = read_until_prompt(port, RESPONSE_TIMEOUT)?;
    protocol::parse_text_frame(&frame, command)
}

fn read_until_prompt(
    port: &mut dyn SerialPort,
    inactivity_timeout: Duration,
) -> anyhow::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut last_byte = Instant::now();
    loop {
        let mut byte = [0u8; 1];
        match port.read(&mut byte) {
            Ok(1) => {
                response.push(byte[0]);
                last_byte = Instant::now();
                if response.ends_with(PROMPT) {
                    return Ok(response);
                }
                if response.len() > MAX_RESPONSE_BYTES {
                    bail!("tinySA response exceeded {MAX_RESPONSE_BYTES} bytes");
                }
            }
            Ok(0) => bail!("tinySA disconnected while returning a response"),
            Ok(_) => unreachable!(),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if last_byte.elapsed() >= inactivity_timeout {
                    bail!("timed out waiting for the tinySA shell prompt");
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to read tinySA response"),
        }
    }
}

fn drain_startup(port: &mut dyn SerialPort) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut last_byte = Instant::now();
    let mut buffer = [0u8; 256];
    loop {
        match port.read(&mut buffer) {
            Ok(count) if count > 0 => last_byte = Instant::now(),
            Ok(_) => return Ok(()),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if last_byte.elapsed() >= DRAIN_QUIET {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to drain tinySA startup output"),
        }
        if started.elapsed() >= RESPONSE_TIMEOUT {
            bail!("tinySA startup output did not become idle");
        }
    }
}

fn scan_segment(
    port: &mut dyn SerialPort,
    command_rx: &Receiver<Command>,
    segment: Segment,
    zero_dbm: i32,
    inactivity_timeout: Duration,
) -> anyhow::Result<ScanResult> {
    let command = format!(
        "scanraw {} {} {} 1",
        segment.start_hz, segment.stop_hz, segment.points
    );
    if let Some(command) = poll_scan_command(command_rx)? {
        return Ok(ScanResult::Interrupted(command, Ok(())));
    }
    port.write_all(command.as_bytes())
        .context("failed to write tinySA scan command")?;
    port.write_all(b"\r")
        .context("failed to terminate tinySA scan command")?;
    port.flush()
        .context("failed to flush tinySA scan command")?;
    let mut expected = command.into_bytes();
    expected.extend_from_slice(b"\r\n{");
    let mut frame = Vec::with_capacity(2 + segment.points as usize * 3);
    for (index, expected_byte) in expected.into_iter().enumerate() {
        match next_scan_byte(port, command_rx, inactivity_timeout)? {
            ByteEvent::Byte(byte) if byte == expected_byte => {}
            ByteEvent::Byte(byte) => {
                bail!(
                    "tinySA scan prelude byte {index} was 0x{byte:02x}, expected 0x{expected_byte:02x}"
                )
            }
            ByteEvent::Command(command) => {
                return Ok(ScanResult::Interrupted(command, abort_active_scan(port)))
            }
        }
    }
    frame.push(b'{');
    for index in 0..segment.points {
        let tag = match next_scan_byte(port, command_rx, inactivity_timeout)? {
            ByteEvent::Byte(byte) => byte,
            ByteEvent::Command(command) => {
                return Ok(ScanResult::Interrupted(command, abort_active_scan(port)))
            }
        };
        if tag == b'}' {
            bail!(
                "tinySA scan closed after {index} of {} records",
                segment.points
            );
        }
        if tag != b'x' {
            bail!("tinySA scan record {index} has malformed tag 0x{tag:02x}");
        }
        frame.push(tag);
        for _ in 0..2 {
            match next_scan_byte(port, command_rx, inactivity_timeout)? {
                ByteEvent::Byte(byte) => frame.push(byte),
                ByteEvent::Command(command) => {
                    return Ok(ScanResult::Interrupted(command, abort_active_scan(port)))
                }
            }
        }
    }
    let close = match next_scan_byte(port, command_rx, inactivity_timeout)? {
        ByteEvent::Byte(byte) => byte,
        ByteEvent::Command(command) => {
            return Ok(ScanResult::Interrupted(command, abort_active_scan(port)))
        }
    };
    frame.push(close);
    let levels_dbm = protocol::parse_scan_frame(&frame, segment.points, zero_dbm)?;
    for expected_byte in PROMPT {
        match next_port_byte(port)? {
            byte if byte == *expected_byte => {}
            byte => bail!(
                "tinySA emitted text after a scan frame: 0x{byte:02x} before the shell prompt"
            ),
        }
    }
    Ok(ScanResult::Complete {
        frequencies_hz: protocol::scan_frequencies(
            segment.start_hz,
            segment.stop_hz,
            segment.points,
        ),
        levels_dbm,
        effective_center_hz: segment.start_hz + (segment.stop_hz - segment.start_hz) / 2,
        effective_span_hz: segment.stop_hz - segment.start_hz,
    })
}

fn next_scan_byte(
    port: &mut dyn SerialPort,
    command_rx: &Receiver<Command>,
    inactivity_timeout: Duration,
) -> anyhow::Result<ByteEvent> {
    let last_byte = Instant::now();
    loop {
        if let Some(command) = poll_scan_command(command_rx)? {
            return Ok(ByteEvent::Command(command));
        }
        let mut byte = [0u8; 1];
        match port.read(&mut byte) {
            Ok(1) => return Ok(ByteEvent::Byte(byte[0])),
            Ok(0) => bail!("tinySA disconnected during a scan"),
            Ok(_) => unreachable!(),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if last_byte.elapsed() >= inactivity_timeout {
                    bail!("tinySA scan timed out");
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to read tinySA scan"),
        }
    }
}

fn poll_scan_command(command_rx: &Receiver<Command>) -> anyhow::Result<Option<Command>> {
    loop {
        match command_rx.try_recv() {
            Ok(Command::IsStreaming(reply)) => {
                let _ = reply.send(Ok(true));
            }
            Ok(command) => return Ok(Some(command)),
            Err(TryRecvError::Disconnected) => bail!("tinySA command channel disconnected"),
            Err(TryRecvError::Empty) => return Ok(None),
        }
    }
}

fn next_port_byte(port: &mut dyn SerialPort) -> anyhow::Result<u8> {
    let last_byte = Instant::now();
    loop {
        let mut byte = [0u8; 1];
        match port.read(&mut byte) {
            Ok(1) => return Ok(byte[0]),
            Ok(0) => bail!("tinySA disconnected during a scan"),
            Ok(_) => unreachable!(),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if last_byte.elapsed() >= RESPONSE_TIMEOUT {
                    bail!("tinySA scan timed out");
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to read tinySA scan"),
        }
    }
}

fn abort_active_scan(port: &mut dyn SerialPort) -> anyhow::Result<()> {
    port.write_all(b"abort\r")
        .context("failed to send tinySA scan abort")?;
    port.flush().context("failed to flush tinySA scan abort")?;
    let started = Instant::now();
    let mut last_byte = Instant::now();
    let mut response = Vec::new();
    loop {
        let mut buffer = [0u8; 64];
        match port.read(&mut buffer) {
            Ok(count) if count > 0 => {
                response.extend_from_slice(&buffer[..count]);
                last_byte = Instant::now();
                if response.len() > MAX_RESPONSE_BYTES {
                    bail!("tinySA abort drain exceeded {MAX_RESPONSE_BYTES} bytes");
                }
            }
            Ok(_) => bail!("tinySA disconnected while aborting a scan"),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if last_byte.elapsed() >= DRAIN_QUIET {
                    if !response
                        .windows(PROMPT.len())
                        .any(|window| window == PROMPT)
                    {
                        bail!("tinySA abort did not return a shell prompt");
                    }
                    return Ok(());
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to drain tinySA aborted scan"),
        }
        if started.elapsed() >= RESPONSE_TIMEOUT {
            bail!("timed out draining the tinySA aborted scan");
        }
    }
}

fn reject_command(command: Command, error: anyhow::Error) {
    let message = error.to_string();
    match command {
        Command::Start(_, reply)
        | Command::Stop(reply)
        | Command::SetFrequency(_, reply)
        | Command::NoOp(reply)
        | Command::Shutdown(reply) => {
            let _ = reply.send(Err(anyhow!(message)));
        }
        Command::IsStreaming(reply) => {
            let _ = reply.send(Err(anyhow!(message)));
        }
        Command::SetSpan(_, reply) => {
            let _ = reply.send(Err(anyhow!(message)));
        }
    }
}

fn capabilities(model: Model) -> DeviceCapabilities {
    DeviceCapabilities {
        acquisition: AcquisitionKind::PowerTrace,
        sample_rate_is_span: true,
        level_unit: LevelUnit::Dbm,
        level_min_db: -120.0,
        level_max_db: 20.0,
        trace_stale_ms: 5_000,
        freq_min_hz: MIN_FREQUENCY_HZ,
        freq_max_hz: model.maximum_hz(),
        sample_rate_min_hz: 1.0,
        sample_rate_max_hz: (model.maximum_hz() - MIN_FREQUENCY_HZ) as f64,
        default_frequency_hz: DEFAULT_FREQUENCY_HZ,
        default_sample_rate_hz: DEFAULT_SPAN_HZ as f64,
        sample_geometry: SampleGeometry {
            format: SampleFormat::Int8,
            full_scale: 1.0,
        },
        gain: GainModel::new(Vec::new(), "RF", "RF").with_gauge_fallback(0),
        samples_per_transfer: 0,
        has_bb_filter: true,
        friis_applicable: false,
        delivery: DeliveryModel::Pull,
    }
}

fn centered_window(center_hz: u64, span_hz: u64, minimum_hz: u64, maximum_hz: u64) -> (u64, u64) {
    let span_hz = span_hz.min(maximum_hz - minimum_hz);
    let mut start_hz = center_hz.saturating_sub(span_hz / 2);
    let mut stop_hz = start_hz.saturating_add(span_hz);
    if start_hz < minimum_hz {
        start_hz = minimum_hz;
        stop_hz = minimum_hz + span_hz;
    }

    if stop_hz > maximum_hz {
        stop_hz = maximum_hz;
        start_hz = maximum_hz - span_hz;
    }
    (start_hz, stop_hz)
}

fn uniform_display_frequencies(measured_hz: &[u64]) -> anyhow::Result<Vec<u64>> {
    let first = *measured_hz
        .first()
        .context("tinySA scan returned no frequencies")?;
    let last = *measured_hz
        .last()
        .context("tinySA scan returned no frequencies")?;
    let intervals = measured_hz.len().saturating_sub(1) as u64;
    if intervals == 0 {
        bail!("tinySA scan returned only one frequency");
    }
    let step = last.saturating_sub(first) / intervals;
    if step == 0 {
        bail!(
            "tinySA scan span is too narrow for {} points",
            measured_hz.len()
        );
    }
    Ok((0..measured_hz.len())
        .map(|index| first + step * index as u64)
        .collect())
}

fn scan_segments(model: Model, start_hz: u64, stop_hz: u64, points: u32) -> Vec<Segment> {
    let boundary = model.path_boundary_hz();
    if start_hz < boundary && boundary < stop_hz && points >= 2 {
        let total_span = stop_hz - start_hz;
        let lower_span = boundary - start_hz;
        let lower_points = ((points as u64 * lower_span + total_span / 2) / total_span)
            .clamp(1, points as u64 - 1) as u32;
        return vec![
            Segment {
                start_hz,
                stop_hz: boundary,
                points: lower_points,
                path: RfPath::Lower,
            },
            Segment {
                start_hz: boundary,
                stop_hz,
                points: points - lower_points,
                path: RfPath::Upper,
            },
        ];
    }
    vec![Segment {
        start_hz,
        stop_hz,
        points,
        path: if start_hz < boundary {
            RfPath::Lower
        } else {
            RfPath::Upper
        },
    }]
}

fn startup_settings(model: Model) -> (ScanSettings, Vec<&'static str>) {
    let spur = if model.is_ultra() {
        SpurMode::Auto
    } else {
        SpurMode::On
    };
    let mut commands = vec!["rbw auto", "attenuate auto"];
    if model.is_ultra() {
        commands.extend(["lna off", "lna2 auto", "agc auto", "spur auto"]);
    } else {
        commands.push("spur on");
    }
    (
        ScanSettings {
            points: DEFAULT_POINTS,
            rbw_khz: None,
            spur,
        },
        commands,
    )
}

fn scan_inactivity_timeout(
    segment: Segment,
    model: Model,
    settings: ScanSettings,
) -> anyhow::Result<Duration> {
    let span_hz = segment.stop_hz.saturating_sub(segment.start_hz) as f64;
    let (minimum_rbw, maximum_rbw) = if model.is_ultra() {
        (0.2, 850.0)
    } else {
        (3.0, 600.0)
    };
    let rbw_khz = settings
        .rbw_khz
        .unwrap_or_else(|| (span_hz * 7e-6).clamp(minimum_rbw, maximum_rbw));
    let points = segment.points.max(1) as f64;
    let mut total_seconds = (span_hz / 20_000.0) / rbw_khz.powi(2) + points / 500.0;
    if (settings.spur == SpurMode::On && segment.stop_hz > 800_000_000)
        || settings.spur == SpurMode::Auto
    {
        total_seconds *= 2.0;
    }
    let block_seconds = total_seconds * 20.0 / points + 1.0;
    Ok(Duration::from_secs_f64(
        block_seconds.max(RESPONSE_TIMEOUT.as_secs_f64()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_scans_split_at_the_model_path_boundary() {
        assert_eq!(
            scan_segments(Model::Basic, 300_000_000, 400_000_000, 290),
            vec![
                Segment {
                    start_hz: 300_000_000,
                    stop_hz: 350_000_000,
                    points: 145,
                    path: RfPath::Lower,
                },
                Segment {
                    start_hz: 350_000_000,
                    stop_hz: 400_000_000,
                    points: 145,
                    path: RfPath::Upper,
                },
            ]
        );
        assert_eq!(
            scan_segments(Model::Zs405, 700_000_000, 900_000_000, 450)[0].stop_hz,
            800_000_000
        );
        assert_eq!(
            scan_segments(Model::Zs407, 800_000_000, 1_000_000_000, 450)[0].stop_hz,
            900_000_000
        );
    }

    #[test]
    fn unsplit_scans_select_the_expected_path() {
        assert_eq!(
            scan_segments(Model::Basic, 100_000, 200_000_000, 64)[0].path,
            RfPath::Lower
        );
        assert_eq!(
            scan_segments(Model::UltraUnknown, 1_000_000_000, 2_000_000_000, 64)[0].path,
            RfPath::Upper
        );
    }

    #[test]
    fn startup_uses_safe_automatic_controls() {
        let (ultra, commands) = startup_settings(Model::Zs407);
        assert_eq!(ultra.points, DEFAULT_POINTS);
        assert_eq!(ultra.rbw_khz, None);
        assert_eq!(
            commands,
            [
                "rbw auto",
                "attenuate auto",
                "lna off",
                "lna2 auto",
                "agc auto",
                "spur auto",
            ]
        );
        let (basic, commands) = startup_settings(Model::Basic);
        assert_eq!(basic.spur, SpurMode::On);
        assert_eq!(commands, ["rbw auto", "attenuate auto", "spur on"]);
    }

    #[test]
    fn centered_windows_keep_the_requested_span_inside_the_model_range() {
        let low_edge = centered_window(100_000, 10_000_000, 100_000, 960_000_000);
        assert_eq!(low_edge, (100_000, 10_100_000));
        assert_eq!(low_edge.0 + (low_edge.1 - low_edge.0) / 2, 5_100_000);
        assert_eq!(
            centered_window(960_000_000, 10_000_000, 100_000, 960_000_000),
            (950_000_000, 960_000_000)
        );
    }

    #[test]
    fn float_rounded_firmware_frequencies_get_a_uniform_display_grid() {
        let measured = protocol::scan_frequencies(100_000_000, 200_000_000, 450);
        assert!(measured
            .windows(2)
            .any(|pair| pair[1] - pair[0] != measured[1] - measured[0]));
        let display = uniform_display_frequencies(&measured).unwrap();
        let step = display[1] - display[0];
        assert!(display.windows(2).all(|pair| pair[1] - pair[0] == step));
        assert_eq!(display[0], measured[0]);
        assert!(display.last().unwrap().abs_diff(*measured.last().unwrap()) < 450);
    }

    #[test]
    fn a_span_too_narrow_for_the_point_count_is_rejected() {
        assert!(uniform_display_frequencies(&[100_000, 100_000, 100_000]).is_err());
    }

    #[test]
    fn an_edge_shift_becomes_the_next_scan_center() {
        let (start, stop) = centered_window(100_000, 10_000_000, 100_000, 960_000_000);
        let effective_center = start + (stop - start) / 2;
        assert_eq!(effective_center, 5_100_000);
        assert_eq!(
            centered_window(effective_center, 1_000_000, 100_000, 960_000_000),
            (4_600_000, 5_600_000)
        );
    }

    #[test]
    fn unknown_ultra_uses_the_conservative_zs405_range() {
        assert_eq!(Model::UltraUnknown.maximum_hz(), 6_000_000_000);
        assert_eq!(Model::UltraUnknown.path_boundary_hz(), 800_000_000);
    }

    #[test]
    fn startup_forces_every_model_into_input_mode() {
        assert_eq!(input_mode_command(Model::Basic), "mode low input");
        for model in [
            Model::Zs405,
            Model::Zs406,
            Model::Zs407,
            Model::UltraUnknown,
        ] {
            assert_eq!(input_mode_command(model), "mode input");
        }
    }

    #[test]
    fn narrow_rbw_expands_the_scan_deadline() {
        let settings = ScanSettings {
            points: DEFAULT_POINTS,
            rbw_khz: Some(0.2),
            spur: SpurMode::Auto,
        };
        let segment = Segment {
            start_hz: 400_000_000,
            stop_hz: 500_000_000,
            points: 450,
            path: RfPath::Lower,
        };
        let timeout = scan_inactivity_timeout(segment, Model::Zs405, settings).unwrap();
        assert!(timeout > Duration::from_secs(120), "{timeout:?}");
    }

    #[cfg(test)]
    mod hardware_tests {
        use super::*;

        #[test]
        #[ignore = "requires SDRTOP_TINYSA_TEST to name a connected serial port"]
        fn connected_device_streams_spectrum_frames() {
            let path = std::env::var("SDRTOP_TINYSA_TEST").expect("SDRTOP_TINYSA_TEST is not set");
            let device = TinySaDevice::open(Path::new(&path)).unwrap();
            assert!(device
                .info()
                .board_name
                .to_ascii_lowercase()
                .contains("tinysa"));

            let state = Arc::new(Mutex::new(crate::state::SdrMetrics::fixture()));
            let (sample_tx, _) = crossbeam_channel::bounded(1);
            let (demod_tx, _) = crossbeam_channel::bounded(1);
            let (net_tx, _) = crossbeam_channel::bounded(1);
            let (power_tx, power_rx) = crossbeam_channel::bounded(4);
            let context = Arc::new(RxContext {
                metrics: Arc::clone(&state),
                sample_tx,
                fft_feed: crate::hardware::FeedHealth::default(),
                demod_tx,
                net_tx,
                net_feed: crate::hardware::FeedHealth::default(),
                power_tx,
                geometry: device.capabilities().sample_geometry,
            });

            device.start_rx(context).unwrap();
            let spectrum = power_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            assert_eq!(spectrum.frequencies_hz.len(), spectrum.levels_dbm.len());
            assert!(!spectrum.frequencies_hz.is_empty());
            device.stop_rx().unwrap();
            assert!(!device.is_streaming());
        }
    }
}
