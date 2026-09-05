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

use crate::config::TinySaSettings;
use crate::hardware::{
    AcquisitionModel, DeliveryModel, DeviceCapabilities, DeviceInfo, DeviceListing, DeviceOption,
    DirectSweepConfig, GainModel, LevelUnit, PowerTrace, PowerTraceTarget, RxContext, SampleFormat,
    SampleGeometry, SdrDevice, SoftwareStack,
};

use super::traits::RateSet;
use protocol::{Identity, Model, PROMPT};

const MIN_FREQUENCY_HZ: u64 = 100_000;
const DEFAULT_FREQUENCY_HZ: u64 = 100_000_000;
const DEFAULT_SPAN_HZ: u64 = 10_000_000;
const READ_TIMEOUT: Duration = Duration::from_millis(50);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const SCAN_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);
const DRAIN_QUIET: Duration = Duration::from_millis(200);
const MAX_RESPONSE_BYTES: usize = 128 * 1024;

type UnitReply = Sender<anyhow::Result<()>>;

pub fn list() -> Vec<DeviceListing> {
    discovery::list()
}

pub struct TinySaDevice {
    caps: DeviceCapabilities,
    info: DeviceInfo,
    notes: Vec<String>,
    options: Arc<Mutex<Vec<DeviceOption>>>,
    command_tx: Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl TinySaDevice {
    pub fn open(path: &Path, settings: &TinySaSettings) -> anyhow::Result<Self> {
        validate_settings_shape(settings)?;
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
        let options = Arc::new(Mutex::new(Vec::new()));
        let initial_settings = settings.clone();
        let worker_options = Arc::clone(&options);
        let worker = thread::Builder::new()
            .name("tinysa-serial".to_string())
            .spawn(move || {
                worker_entry(port, command_rx, init_tx, initial_settings, worker_options)
            })
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
        let mut notes = Vec::new();
        if let Some(hardware) = &initialized.identity.hardware {
            notes.push(hardware.clone());
        }
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
            options,
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

    fn set_direct_sweep(&self, config: Option<DirectSweepConfig>) -> anyhow::Result<()> {
        self.request(|reply| Command::SetDirectSweep(config, reply))
    }

    fn options(&self) -> Vec<DeviceOption> {
        self.options
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_option(&self, id: &str, value: &str) -> anyhow::Result<()> {
        self.request(|reply| Command::SetOption {
            id: id.to_string(),
            value: value.to_string(),
            reply,
        })
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
    SetDirectSweep(Option<DirectSweepConfig>, UnitReply),
    SetOption {
        id: String,
        value: String,
        reply: UnitReply,
    },
    Shutdown(UnitReply),
}

struct Initialized {
    identity: Identity,
}

struct Worker {
    port: Box<dyn SerialPort>,
    command_rx: Receiver<Command>,
    identity: Identity,
    options: Vec<DeviceOption>,
    option_state: Arc<Mutex<Vec<DeviceOption>>>,
    center_hz: u64,
    span_hz: u64,
    direct_sweep: Option<DirectSweepConfig>,
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
    settings: TinySaSettings,
    option_state: Arc<Mutex<Vec<DeviceOption>>>,
) {
    let initialized = initialize(&mut *port, &settings);
    let (identity, options) = match initialized {
        Ok(value) => value,
        Err(error) => {
            let _ = init_tx.send(Err(error));
            return;
        }
    };
    *option_state
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = options.clone();
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
        options,
        option_state,
        center_hz: DEFAULT_FREQUENCY_HZ,
        span_hz: DEFAULT_SPAN_HZ,
        direct_sweep: None,
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
                }) => {
                    if let Some(context) = &self.rx_context {
                        let target = if self.direct_sweep.is_some() {
                            PowerTraceTarget::Sweep
                        } else {
                            PowerTraceTarget::Spectrum
                        };
                        let _ = context.power_tx.try_send(PowerTrace {
                            target,
                            generation: self
                                .direct_sweep
                                .map(|config| config.generation)
                                .unwrap_or(0),
                            frequencies_hz,
                            levels_dbm,
                            rbw_hz: current_rbw_hz(&self.options),
                        });
                    }
                    while let Ok(command) = self.command_rx.try_recv() {
                        if !self.handle_command(command) {
                            return;
                        }
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
                    if !self.handle_command(command) {
                        return;
                    }
                    while let Ok(command) = self.command_rx.try_recv() {
                        if !self.handle_command(command) {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = abort_active_scan(&mut *self.port);
                    self.stop_acquisition(&error.to_string());
                }
            }
        }
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
                        current_rbw_hz(&self.options).unwrap_or(0),
                    ))
                };
                let _ = reply.send(result);
            }
            Command::NoOp(reply) => {
                let _ = reply.send(Ok(()));
            }
            Command::SetDirectSweep(config, reply) => {
                let result = validate_direct_sweep(config, self.identity.model)
                    .map(|()| self.direct_sweep = config);
                let _ = reply.send(result);
            }
            Command::SetOption { id, value, reply } => {
                let result = self.apply_option(&id, &value);
                let _ = reply.send(result);
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

    fn apply_option(&mut self, id: &str, value: &str) -> anyhow::Result<()> {
        let selected = validated_option_index(&self.options, id, value)?;
        if id == "attenuation"
            && value != "0"
            && selected_option_value(&self.options, "lna") == Some("on")
        {
            bail!("turn the tinySA LNA off before changing attenuation");
        }
        if let Some(command) = settings_command(self.identity.model, id, value)? {
            send_text_command(&mut *self.port, &command)?;
        }
        let option = self
            .options
            .iter_mut()
            .find(|option| option.id == id)
            .context("tinySA option disappeared during update")?;
        option.selected = selected;
        if id == "lna" && value == "on" {
            set_selected_option(&mut self.options, "attenuation", "0")?;
        }
        *self
            .option_state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = self.options.clone();
        Ok(())
    }

    fn scan_once(&mut self) -> anyhow::Result<ScanResult> {
        let (start_hz, stop_hz) = self.scan_window();
        let points = selected_points(&self.options)?;
        let mut frequencies_hz = Vec::with_capacity(points as usize);
        let mut levels_dbm = Vec::with_capacity(points as usize);
        for segment in scan_segments(self.identity.model, start_hz, stop_hz, points) {
            if let Some(command) = poll_scan_command(&self.command_rx)? {
                return Ok(ScanResult::Interrupted(command, Ok(())));
            }
            self.select_path(segment.path)?;
            match scan_segment(
                &mut *self.port,
                &self.command_rx,
                segment,
                self.identity.zero_dbm,
            )? {
                ScanResult::Complete {
                    frequencies_hz: segment_frequencies,
                    levels_dbm: segment_levels,
                } => {
                    frequencies_hz.extend(segment_frequencies);
                    levels_dbm.extend(segment_levels);
                }
                interrupted => return Ok(interrupted),
            }
        }
        Ok(ScanResult::Complete {
            frequencies_hz,
            levels_dbm,
        })
    }

    fn scan_window(&self) -> (u64, u64) {
        if let Some(config) = self.direct_sweep {
            return (config.start_hz, config.stop_hz);
        }
        centered_window(
            self.center_hz,
            self.span_hz,
            MIN_FREQUENCY_HZ,
            self.identity.model.maximum_hz(),
        )
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
        self.active_path = Some(path);
        Ok(())
    }
}

fn initialize(
    port: &mut dyn SerialPort,
    settings: &TinySaSettings,
) -> anyhow::Result<(Identity, Vec<DeviceOption>)> {
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
    let info = send_text_command(port, "info")?;
    let help = send_text_command(port, "help")?;
    let zero = send_text_command(port, "zero")?;
    let identity = protocol::parse_identity(&version, &info, &help, &zero)?;
    send_text_command(port, "abort on")?;
    let (options, commands) = startup_options(identity.model, settings)?;
    for command in commands {
        send_text_command(port, &command)?;
    }
    Ok((identity, options))
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
        match next_scan_byte(port, command_rx)? {
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
        let tag = match next_scan_byte(port, command_rx)? {
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
            match next_scan_byte(port, command_rx)? {
                ByteEvent::Byte(byte) => frame.push(byte),
                ByteEvent::Command(command) => {
                    return Ok(ScanResult::Interrupted(command, abort_active_scan(port)))
                }
            }
        }
    }
    let close = match next_scan_byte(port, command_rx)? {
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
            byte => {
                bail!(
                    "tinySA emitted text after a scan frame: 0x{byte:02x} before the shell prompt"
                )
            }
        }
    }
    Ok(ScanResult::Complete {
        frequencies_hz: protocol::scan_frequencies(
            segment.start_hz,
            segment.stop_hz,
            segment.points,
        ),
        levels_dbm,
    })
}

fn next_scan_byte(
    port: &mut dyn SerialPort,
    command_rx: &Receiver<Command>,
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
                if last_byte.elapsed() >= SCAN_INACTIVITY_TIMEOUT {
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
        | Command::SetDirectSweep(_, reply)
        | Command::SetOption { reply, .. }
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
        acquisition: AcquisitionModel::PowerSweep,
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

fn validate_direct_sweep(config: Option<DirectSweepConfig>, model: Model) -> anyhow::Result<()> {
    let Some(config) = config else {
        return Ok(());
    };
    if config.start_hz < MIN_FREQUENCY_HZ
        || config.stop_hz > model.maximum_hz()
        || config.start_hz >= config.stop_hz
    {
        bail!(
            "tinySA sweep must be within {}..{} Hz with start below stop",
            MIN_FREQUENCY_HZ,
            model.maximum_hz()
        );
    }
    Ok(())
}

fn validate_settings_shape(settings: &TinySaSettings) -> anyhow::Result<()> {
    if !allowed_points().contains(&settings.points) {
        bail!("tinySA points must be one of 64, 128, 290, 450, 900, or 1800");
    }
    if settings.ext_gain_db < -100 || settings.ext_gain_db > 100 {
        bail!("tinySA external gain must be within -100..100 dB");
    }
    Ok(())
}

fn startup_options(
    model: Model,
    settings: &TinySaSettings,
) -> anyhow::Result<(Vec<DeviceOption>, Vec<String>)> {
    validate_settings_shape(settings)?;
    let mut options = option_definitions(model);
    let spur = if model == Model::Basic && settings.spur == "auto" {
        "on".to_string()
    } else {
        settings.spur.clone()
    };
    let attenuation = if model.is_ultra() && settings.lna {
        "0".to_string()
    } else {
        settings.attenuation.clone()
    };
    let rbw = if model == Model::Basic && ["0.2", "1", "850"].contains(&settings.rbw.as_str()) {
        "auto".to_string()
    } else {
        settings.rbw.clone()
    };
    let mut values = vec![
        ("points", settings.points.to_string()),
        ("rbw", rbw),
        ("attenuation", attenuation),
    ];
    if model.is_ultra() {
        values.extend([
            ("lna", if settings.lna { "on" } else { "off" }.to_string()),
            ("lna2", settings.lna2.clone()),
            ("agc", settings.agc.clone()),
        ]);
    }
    values.extend([
        ("spur", spur),
        ("ext_gain", settings.ext_gain_db.to_string()),
    ]);
    let mut commands = Vec::new();
    for (id, value) in values {
        let selected = validated_option_index(&options, id, &value)?;
        if let Some(command) = settings_command(model, id, &value)? {
            commands.push(command);
        }
        options
            .iter_mut()
            .find(|option| option.id == id)
            .context("tinySA startup option is missing")?
            .selected = selected;
    }
    Ok((options, commands))
}

fn option_definitions(model: Model) -> Vec<DeviceOption> {
    let mut options = vec![
        option(
            "points",
            "Points",
            allowed_points().map(|value| value.to_string()).to_vec(),
        ),
        option(
            "rbw",
            "RBW (kHz)",
            if model.is_ultra() {
                [
                    "auto", "0.2", "1", "3", "10", "30", "100", "300", "600", "850",
                ]
                .map(str::to_string)
                .to_vec()
            } else {
                ["auto", "3", "10", "30", "100", "300", "600"]
                    .map(str::to_string)
                    .to_vec()
            },
        ),
        option(
            "attenuation",
            "Attenuation (dB)",
            std::iter::once("auto".to_string())
                .chain((0..=31).map(|value| value.to_string()))
                .collect(),
        ),
    ];
    if model.is_ultra() {
        options.extend([
            option("lna", "LNA", strings(&["off", "on"])),
            option(
                "lna2",
                "LNA2",
                std::iter::once("auto".to_string())
                    .chain((0..=7).map(|value| value.to_string()))
                    .collect(),
            ),
            option(
                "agc",
                "AGC",
                std::iter::once("auto".to_string())
                    .chain((0..=7).map(|value| value.to_string()))
                    .collect(),
            ),
        ]);
    }
    options.push(option(
        "spur",
        "Spur removal",
        if model.is_ultra() {
            strings(&["off", "on", "auto"])
        } else {
            strings(&["off", "on"])
        },
    ));
    options.push(option(
        "ext_gain",
        "External gain (dB)",
        (-100..=100).map(|value| value.to_string()).collect(),
    ));
    options
}

fn option(id: &str, label: &str, values: Vec<String>) -> DeviceOption {
    DeviceOption {
        id: id.to_string(),
        label: label.to_string(),
        values,
        selected: 0,
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn allowed_points() -> [u32; 6] {
    [64, 128, 290, 450, 900, 1800]
}

fn validated_option_index(
    options: &[DeviceOption],
    id: &str,
    value: &str,
) -> anyhow::Result<usize> {
    let option = options
        .iter()
        .find(|option| option.id == id)
        .with_context(|| format!("unknown tinySA option {id}"))?;
    option
        .values
        .iter()
        .position(|candidate| candidate == value)
        .with_context(|| format!("invalid value {value:?} for tinySA option {id}"))
}

fn selected_option_value<'a>(options: &'a [DeviceOption], id: &str) -> Option<&'a str> {
    options
        .iter()
        .find(|option| option.id == id)
        .and_then(DeviceOption::selected_value)
}

fn set_selected_option(options: &mut [DeviceOption], id: &str, value: &str) -> anyhow::Result<()> {
    let selected = validated_option_index(options, id, value)?;
    options
        .iter_mut()
        .find(|option| option.id == id)
        .with_context(|| format!("unknown tinySA option {id}"))?
        .selected = selected;
    Ok(())
}

fn settings_command(model: Model, id: &str, value: &str) -> anyhow::Result<Option<String>> {
    let command = match id {
        "points" => return Ok(None),
        "rbw" => format!("rbw {value}"),
        "attenuation" => format!("attenuate {value}"),
        "lna" if model.is_ultra() => format!("lna {value}"),
        "lna2" if model.is_ultra() => format!("lna2 {value}"),
        "agc" if model.is_ultra() => format!("agc {value}"),
        "spur" => format!("spur {value}"),
        "ext_gain" => format!("ext_gain {value}"),
        _ => bail!("unknown tinySA option {id}"),
    };
    Ok(Some(command))
}

fn selected_points(options: &[DeviceOption]) -> anyhow::Result<u32> {
    options
        .iter()
        .find(|option| option.id == "points")
        .and_then(DeviceOption::selected_value)
        .context("tinySA point setting is missing")?
        .parse()
        .context("tinySA point setting is invalid")
}

fn current_rbw_hz(options: &[DeviceOption]) -> Option<u32> {
    let value = options
        .iter()
        .find(|option| option.id == "rbw")?
        .selected_value()?;
    if value == "auto" {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .map(|khz| (khz * 1_000.0).round() as u32)
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

    #[cfg(test)]
    mod hardware_tests {
        use super::*;
        use std::time::Duration;

        #[test]
        #[ignore = "requires SDRTOP_TINYSA_TEST to name a connected serial port"]
        fn connected_device_streams_spectrum_and_sweep_frames() {
            let path = std::env::var("SDRTOP_TINYSA_TEST").expect("SDRTOP_TINYSA_TEST is not set");
            let device = TinySaDevice::open(Path::new(&path), &TinySaSettings::default()).unwrap();
            assert!(device
                .info()
                .board_name
                .to_ascii_lowercase()
                .contains("tinysa"));
            assert!(device.options().iter().any(|option| option.id == "rbw"));

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
            assert_eq!(spectrum.target, PowerTraceTarget::Spectrum);
            assert_eq!(spectrum.frequencies_hz.len(), spectrum.levels_dbm.len());
            assert!(!spectrum.frequencies_hz.is_empty());

            device
                .set_direct_sweep(Some(DirectSweepConfig {
                    start_hz: 100_000_000,
                    stop_hz: 101_000_000,
                    dwell_ms: 0,
                    generation: 1,
                }))
                .unwrap();
            let sweep = power_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            assert_eq!(sweep.target, PowerTraceTarget::Sweep);
            assert_eq!(sweep.frequencies_hz.len(), sweep.levels_dbm.len());

            device.set_option("points", "64").unwrap();
            if device.options().iter().any(|option| option.id == "lna") {
                device.set_option("attenuation", "12").unwrap();
                device.set_option("lna", "on").unwrap();
                let options = device.options();
                assert_eq!(selected_option_value(&options, "attenuation"), Some("0"));
                assert!(device.set_option("attenuation", "auto").is_err());
                device.set_option("lna", "off").unwrap();
                device.set_option("attenuation", "auto").unwrap();
            }
            if device.capabilities().freq_max_hz > 1_001_000_000 {
                device
                    .set_direct_sweep(Some(DirectSweepConfig {
                        start_hz: 1_000_000_000,
                        stop_hz: 1_001_000_000,
                        dwell_ms: 0,
                        generation: 2,
                    }))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let high = power_rx.recv_timeout(remaining).unwrap_or_else(|error| {
                        let log = state
                            .lock()
                            .unwrap()
                            .ui
                            .log
                            .iter()
                            .map(|entry| entry.text.to_string())
                            .collect::<Vec<_>>();
                        panic!("{error}: {log:?}");
                    });
                    if high.frequencies_hz.first().copied() == Some(1_000_000_000) {
                        break;
                    }
                }
                device.set_direct_sweep(None).unwrap();
                device.set_frequency(100_000_000).unwrap();
                while power_rx
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .target
                    != PowerTraceTarget::Spectrum
                {}
            }
            device.stop_rx().unwrap();
            assert!(!device.is_streaming());
        }
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
    fn option_validation_rejects_unknown_ids_and_values() {
        let options = option_definitions(Model::Basic);
        assert!(validated_option_index(&options, "points", "450").is_ok());
        assert!(validated_option_index(&options, "points", "451").is_err());
        assert!(validated_option_index(&options, "lna", "on").is_err());
        assert!(validated_option_index(&options, "missing", "on").is_err());
    }

    #[test]
    fn settings_map_to_firmware_commands() {
        let settings = TinySaSettings {
            points: 900,
            rbw: "0.2".to_string(),
            attenuation: "12".to_string(),
            lna: true,
            lna2: "3".to_string(),
            agc: "auto".to_string(),
            spur: "auto".to_string(),
            ext_gain_db: -7,
        };
        let (options, commands) = startup_options(Model::Zs407, &settings).unwrap();
        assert_eq!(
            commands,
            [
                "rbw 0.2",
                "attenuate 0",
                "lna on",
                "lna2 3",
                "agc auto",
                "spur auto",
                "ext_gain -7",
            ]
        );
        assert_eq!(
            selected_option_value(&options, "attenuation"),
            Some("0"),
            "enabling the LNA forces physical attenuation to zero"
        );
        let basic = TinySaSettings::default();
        let (options, commands) = startup_options(Model::Basic, &basic).unwrap();
        assert!(commands.iter().any(|command| command == "spur on"));
        assert_eq!(
            options
                .iter()
                .find(|option| option.id == "spur")
                .and_then(DeviceOption::selected_value),
            Some("on")
        );
    }

    #[test]
    fn all_setting_ranges_are_enforced_before_commands_are_built() {
        let mut settings = TinySaSettings {
            rbw: "1".to_string(),
            ..TinySaSettings::default()
        };
        let (options, commands) = startup_options(Model::Basic, &settings).unwrap();
        assert_eq!(selected_option_value(&options, "rbw"), Some("auto"));
        assert!(commands.iter().any(|command| command == "rbw auto"));
        settings.rbw = "3".to_string();
        settings.attenuation = "32".to_string();
        assert!(startup_options(Model::Basic, &settings).is_err());
        settings.attenuation = "auto".to_string();
        settings.ext_gain_db = 101;
        assert!(startup_options(Model::Basic, &settings).is_err());
    }

    #[test]
    fn centered_windows_keep_the_requested_span_inside_the_model_range() {
        assert_eq!(
            centered_window(100_000, 10_000_000, 100_000, 960_000_000),
            (100_000, 10_100_000)
        );
        assert_eq!(
            centered_window(960_000_000, 10_000_000, 100_000, 960_000_000),
            (950_000_000, 960_000_000)
        );
    }
}
