// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

mod discovery;
mod protocol;

use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};

use crate::hardware::{
    AcquisitionKind, DeliveryModel, DeviceCapabilities, DeviceInfo, DeviceListing,
    DirectSweepConfig, GainModel, LevelUnit, PowerTrace, PowerTraceTarget, RxContext, SampleFormat,
    SampleGeometry, SdrDevice, SoftwareStack,
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
const BASIC_LOW_MAX_HZ: u64 = 350_000_000;
const BASIC_HIGH_MIN_HZ: u64 = 240_000_000;
const BASIC_HIGH_MAX_HZ: u64 = 959_000_000;

type UnitReply = Sender<anyhow::Result<()>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BasicInput {
    #[default]
    Low,
    High,
}

impl BasicInput {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            _ => bail!("tinySA input must be 'low' or 'high'"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Low => "LOW",
            Self::High => "HIGH",
        }
    }

    fn mode_command(self) -> &'static str {
        match self {
            Self::Low => "mode low input",
            Self::High => "mode high input",
        }
    }

    fn range(self) -> (u64, u64) {
        match self {
            Self::Low => (MIN_FREQUENCY_HZ, BASIC_LOW_MAX_HZ),
            Self::High => (BASIC_HIGH_MIN_HZ, BASIC_HIGH_MAX_HZ),
        }
    }
}

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

pub fn list(selector: Option<&str>) -> Vec<DeviceListing> {
    let (path, input) = match selector {
        Some(selector) => match parse_selector(selector) {
            Ok(selection) => selection,
            Err(error) => {
                eprintln!("tinySA: invalid device selector: {error}");
                return Vec::new();
            }
        },
        None => (None, None),
    };
    if let Some(path) = path {
        let input_label = input
            .map(|input| format!(" · {} input", input.label()))
            .unwrap_or_default();
        return vec![DeviceListing {
            kind: crate::hardware::DeviceKind::TinySa,
            index: 0,
            label: format!("tinySA · {}{input_label}", path.display()),
            serial: None,
            args: None,
            path: Some(path),
            tiny_sa_input: input,
        }];
    }
    let mut devices = discovery::list();
    for device in &mut devices {
        device.tiny_sa_input = input;
        if let Some(input) = input {
            device
                .label
                .push_str(&format!(" · {} input", input.label()));
        }
    }
    devices
}

pub fn parse_selector(selector: &str) -> anyhow::Result<(Option<PathBuf>, Option<BasicInput>)> {
    let (path, input) = match selector.rsplit_once("?input=") {
        Some((path, input)) => (path, Some(BasicInput::parse(input)?)),
        None => (selector, None),
    };
    if path.contains('?') {
        bail!("tinySA selector only supports '?input=low' or '?input=high'");
    }
    Ok(((!path.is_empty()).then(|| PathBuf::from(path)), input))
}

pub struct TinySaDevice {
    caps: DeviceCapabilities,
    info: DeviceInfo,
    notes: Vec<String>,
    command_tx: Sender<Command>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl TinySaDevice {
    pub fn open(path: &Path, basic_input: BasicInput) -> anyhow::Result<Self> {
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
            .spawn(move || worker_entry(port, command_rx, init_tx, basic_input))
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
        let caps = capabilities(initialized.identity.model, basic_input);
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

    fn set_direct_sweep(&self, config: Option<DirectSweepConfig>) -> anyhow::Result<()> {
        self.request(|reply| Command::SetDirectSweep(config, reply))
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
    basic_input: BasicInput,
    center_hz: u64,
    span_hz: u64,
    direct_sweep: Option<DirectSweepConfig>,
    rx_context: Option<Arc<RxContext>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    start_hz: u64,
    stop_hz: u64,
    points: u32,
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

#[derive(Clone, Copy)]
enum ScanDrain {
    Records(u32),
    FrameClosed,
}

fn worker_entry(
    mut port: Box<dyn SerialPort>,
    command_rx: Receiver<Command>,
    init_tx: Sender<anyhow::Result<Initialized>>,
    basic_input: BasicInput,
) {
    let (identity, settings) = match initialize(&mut *port, basic_input) {
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
    let center_hz = default_frequency(identity.model, basic_input);
    Worker {
        port,
        command_rx,
        identity,
        settings,
        center_hz,
        span_hz: DEFAULT_SPAN_HZ,
        basic_input,
        direct_sweep: None,
        rx_context: None,
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
                    if self.direct_sweep.is_none() {
                        self.center_hz = effective_center_hz;
                        self.span_hz = effective_span_hz;
                    }
                    if let Some(context) = &self.rx_context {
                        let target = if self.direct_sweep.is_some() {
                            PowerTraceTarget::Sweep
                        } else {
                            PowerTraceTarget::Spectrum
                        };
                        let published = context
                            .power_tx
                            .try_send(PowerTrace {
                                target,
                                generation: self
                                    .direct_sweep
                                    .map(|config| config.generation)
                                    .unwrap_or(0),
                                frequencies_hz,
                                levels_dbm,
                                rbw_hz: self
                                    .settings
                                    .rbw_khz
                                    .map(|khz| (khz * 1_000.0).round() as u32),
                            })
                            .is_ok();
                        if published && target == PowerTraceTarget::Spectrum {
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
                        let message = error.to_string();
                        self.stop_acquisition(&message);
                        reject_command(command, anyhow!(message));
                        return;
                    }
                    if !self.handle_command(command) || !self.drain_commands() {
                        return;
                    }
                }
                Err(error) => {
                    best_effort_abort(&mut *self.port);
                    self.stop_acquisition(&error.to_string());
                    return;
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
                let (minimum, maximum) = frequency_range(self.identity.model, self.basic_input);
                self.center_hz = hz.clamp(minimum, maximum);
                let _ = reply.send(Ok(()));
            }
            Command::SetSpan(hz, reply) => {
                let (minimum, maximum) = frequency_range(self.identity.model, self.basic_input);
                let maximum = maximum - minimum;
                let result = normalize_span(hz, maximum, self.settings.points).map(|span_hz| {
                    self.span_hz = span_hz;
                    RateSet::new(
                        hz,
                        Some(self.span_hz as f64),
                        self.settings
                            .rbw_khz
                            .map(|khz| (khz * 1_000.0).round() as u32)
                            .unwrap_or(0),
                    )
                });
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
        let (minimum_hz, maximum_hz) = frequency_range(self.identity.model, self.basic_input);
        let (start_hz, stop_hz) = self
            .direct_sweep
            .map(|config| (config.start_hz, config.stop_hz))
            .unwrap_or_else(|| {
                centered_window(self.center_hz, self.span_hz, minimum_hz, maximum_hz)
            });
        let points = self.settings.points;
        self.center_hz = window_center(start_hz, stop_hz);
        self.span_hz = stop_hz - start_hz;
        if self.span_hz < points as u64 {
            bail!("tinySA scan span is too narrow for {points} points");
        }
        let mut frequencies_hz = Vec::with_capacity(points as usize);
        let mut levels_dbm = Vec::with_capacity(points as usize);
        let segment = Segment {
            start_hz,
            stop_hz,
            points,
        };
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
        let frequencies_hz = display_frequencies(&frequencies_hz)?;
        Ok(ScanResult::Complete {
            frequencies_hz,
            levels_dbm,
            effective_center_hz: self.center_hz,
            effective_span_hz: self.span_hz,
        })
    }
}

fn initialize(
    port: &mut dyn SerialPort,
    basic_input: BasicInput,
) -> anyhow::Result<(Identity, ScanSettings)> {
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
    if identity.model.is_ultra() && basic_input == BasicInput::High {
        bail!("tinySA HIGH input selection applies only to the basic model");
    }
    send_text_command(port, "abort on")?;
    send_text_command(port, input_mode_command(identity.model, basic_input))?;
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

fn input_mode_command(model: Model, basic_input: BasicInput) -> &'static str {
    if model.is_ultra() {
        "mode input"
    } else {
        basic_input.mode_command()
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
        let byte = next_port_byte(port, inactivity_timeout)?;
        if byte != expected_byte {
            bail!(
                "tinySA scan prelude byte {index} was 0x{byte:02x}, expected 0x{expected_byte:02x}"
            );
        }
    }
    frame.push(b'{');
    for index in 0..segment.points {
        let tag = match next_scan_byte(port, command_rx, inactivity_timeout)? {
            ByteEvent::Byte(byte) => byte,
            ByteEvent::Command(command) => {
                return Ok(ScanResult::Interrupted(
                    command,
                    abort_active_scan(
                        port,
                        ScanDrain::Records(segment.points - index),
                        inactivity_timeout,
                    ),
                ))
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
            frame.push(next_port_byte(port, inactivity_timeout)?);
        }
    }
    let close = match next_scan_byte(port, command_rx, inactivity_timeout)? {
        ByteEvent::Byte(byte) => byte,
        ByteEvent::Command(command) => {
            return Ok(ScanResult::Interrupted(
                command,
                abort_active_scan(port, ScanDrain::Records(0), inactivity_timeout),
            ))
        }
    };
    frame.push(close);
    let levels_dbm = protocol::parse_scan_frame(&frame, segment.points, zero_dbm)?;
    for expected_byte in PROMPT {
        match next_scan_byte(port, command_rx, inactivity_timeout)? {
            ByteEvent::Byte(byte) if byte == *expected_byte => {}
            ByteEvent::Byte(byte) => bail!(
                "tinySA emitted text after a scan frame: 0x{byte:02x} before the shell prompt"
            ),
            ByteEvent::Command(command) => {
                return Ok(ScanResult::Interrupted(
                    command,
                    abort_active_scan(port, ScanDrain::FrameClosed, inactivity_timeout),
                ))
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
        effective_center_hz: window_center(segment.start_hz, segment.stop_hz),
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

fn next_port_byte(port: &mut dyn SerialPort, inactivity_timeout: Duration) -> anyhow::Result<u8> {
    let last_byte = Instant::now();
    loop {
        let mut byte = [0u8; 1];
        match port.read(&mut byte) {
            Ok(1) => return Ok(byte[0]),
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

fn abort_active_scan<P>(
    port: &mut P,
    progress: ScanDrain,
    inactivity_timeout: Duration,
) -> anyhow::Result<()>
where
    P: Read + Write + ?Sized,
{
    const ABORT_REPLY: &[u8] = b"abort\r\nch> ";

    port.write_all(b"abort\r")
        .context("failed to send tinySA scan abort")?;
    port.flush().context("failed to flush tinySA scan abort")?;
    let mut records = match progress {
        ScanDrain::Records(records) => Some(records),
        ScanDrain::FrameClosed => None,
    };
    let mut abort_reply_at = 0;
    let mut bytes_read = 0usize;
    let mut last_byte = Instant::now();

    while records.is_some() || abort_reply_at < ABORT_REPLY.len() {
        let byte = read_abort_byte(port, inactivity_timeout, &mut last_byte, &mut bytes_read)?;
        if let Some(remaining) = records {
            match byte {
                b'x' if remaining > 0 => {
                    read_abort_byte(port, inactivity_timeout, &mut last_byte, &mut bytes_read)?;
                    read_abort_byte(port, inactivity_timeout, &mut last_byte, &mut bytes_read)?;
                    records = Some(remaining - 1);
                }
                b'}' => records = None,
                b'a' => {
                    for expected in &ABORT_REPLY[1..] {
                        let byte = read_abort_byte(
                            port,
                            inactivity_timeout,
                            &mut last_byte,
                            &mut bytes_read,
                        )?;
                        if byte != *expected {
                            bail!("tinySA abort acknowledgement was malformed");
                        }
                    }
                    abort_reply_at = ABORT_REPLY.len();
                }
                _ => bail!("tinySA aborted scan frame was malformed"),
            }
        } else if byte == ABORT_REPLY[abort_reply_at] {
            abort_reply_at += 1;
        } else {
            abort_reply_at = usize::from(byte == ABORT_REPLY[0]);
        }
    }
    Ok(())
}

fn read_abort_byte<P>(
    port: &mut P,
    inactivity_timeout: Duration,
    last_byte: &mut Instant,
    bytes_read: &mut usize,
) -> anyhow::Result<u8>
where
    P: Read + ?Sized,
{
    loop {
        let mut byte = [0u8; 1];
        match port.read(&mut byte) {
            Ok(1) => {
                *last_byte = Instant::now();
                *bytes_read += 1;
                if *bytes_read > MAX_RESPONSE_BYTES {
                    bail!("tinySA abort drain exceeded {MAX_RESPONSE_BYTES} bytes");
                }
                return Ok(byte[0]);
            }
            Ok(0) => bail!("tinySA disconnected while aborting a scan"),
            Ok(_) => unreachable!(),
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if last_byte.elapsed() >= inactivity_timeout {
                    bail!("timed out draining the tinySA aborted scan");
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("failed to drain tinySA aborted scan"),
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

fn capabilities(model: Model, basic_input: BasicInput) -> DeviceCapabilities {
    let (minimum_hz, maximum_hz) = frequency_range(model, basic_input);
    DeviceCapabilities {
        acquisition: AcquisitionKind::PowerTrace,
        sample_rate_is_span: true,
        level_unit: LevelUnit::Dbm,
        level_min_db: -120.0,
        level_max_db: 20.0,
        trace_stale_ms: 5_000,
        freq_min_hz: minimum_hz,
        freq_max_hz: maximum_hz,
        sample_rate_min_hz: DEFAULT_POINTS as f64,
        sample_rate_max_hz: (maximum_hz - minimum_hz) as f64,
        default_frequency_hz: default_frequency(model, basic_input),
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

fn frequency_range(model: Model, basic_input: BasicInput) -> (u64, u64) {
    if model.is_ultra() {
        (MIN_FREQUENCY_HZ, model.maximum_hz())
    } else {
        basic_input.range()
    }
}

fn default_frequency(model: Model, basic_input: BasicInput) -> u64 {
    let (minimum_hz, maximum_hz) = frequency_range(model, basic_input);
    DEFAULT_FREQUENCY_HZ.clamp(minimum_hz, maximum_hz)
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

fn window_center(start_hz: u64, stop_hz: u64) -> u64 {
    start_hz + (stop_hz - start_hz) / 2
}

fn normalize_span(hz: f64, maximum_hz: u64, points: u32) -> anyhow::Result<u64> {
    if !hz.is_finite() || hz <= 0.0 {
        bail!("tinySA span must be a positive finite value");
    }
    Ok((hz.round() as u64).clamp(points as u64, maximum_hz))
}

fn display_frequencies(measured_hz: &[u64]) -> anyhow::Result<Vec<u64>> {
    let first = *measured_hz
        .first()
        .context("tinySA scan returned no frequencies")?;
    let last = *measured_hz
        .last()
        .context("tinySA scan returned no frequencies")?;
    if measured_hz.windows(2).any(|pair| pair[1] <= pair[0]) {
        bail!("tinySA scan returned duplicate or descending frequencies");
    }
    let intervals = measured_hz.len().saturating_sub(1) as u64;
    let span = last - first;
    if intervals == 0 || span < intervals {
        bail!(
            "tinySA scan span is too narrow for {} points",
            measured_hz.len()
        );
    }
    Ok((0..measured_hz.len())
        .map(|index| {
            let offset = (span as u128 * index as u128 + intervals as u128 / 2) / intervals as u128;
            first + offset as u64
        })
        .collect())
}

fn startup_settings(model: Model) -> (ScanSettings, Vec<&'static str>) {
    let spur = if model.is_ultra() {
        SpurMode::Auto
    } else {
        SpurMode::On
    };
    let mut commands = Vec::new();
    if model.is_ultra() {
        commands.extend(["ultra on", "ultra auto"]);
    }
    commands.extend(["rbw auto", "attenuate auto"]);
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
    use std::collections::VecDeque;
    use std::io;

    use super::*;

    enum ReadStep {
        Bytes(VecDeque<u8>),
        Delay(Duration),
        FrameClose,
    }

    struct ScriptedPeer {
        reads: VecDeque<ReadStep>,
        writes: Vec<u8>,
        frame_closed: bool,
    }

    impl ScriptedPeer {
        fn new(steps: impl IntoIterator<Item = ReadStep>) -> Self {
            Self {
                reads: steps.into_iter().collect(),
                writes: Vec::new(),
                frame_closed: false,
            }
        }
    }

    impl Read for ScriptedPeer {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                match self.reads.front_mut() {
                    Some(ReadStep::Bytes(bytes)) => {
                        let Some(byte) = bytes.pop_front() else {
                            self.reads.pop_front();
                            continue;
                        };
                        buffer[0] = byte;
                        return Ok(1);
                    }
                    Some(ReadStep::Delay(duration)) => {
                        let duration = *duration;
                        self.reads.pop_front();
                        std::thread::sleep(duration);
                        return Err(io::Error::new(ErrorKind::TimedOut, "scripted delay"));
                    }
                    Some(ReadStep::FrameClose) => {
                        self.reads.pop_front();
                        self.frame_closed = true;
                        buffer[0] = b'}';
                        return Ok(1);
                    }
                    None => return Err(io::Error::new(ErrorKind::TimedOut, "script exhausted")),
                }
            }
        }
    }

    impl Write for ScriptedPeer {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if buffer == b"next\r" {
                assert!(
                    self.frame_closed,
                    "next request preceded the old frame close"
                );
            }
            self.writes.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn bytes(value: &[u8]) -> ReadStep {
        ReadStep::Bytes(value.iter().copied().collect())
    }

    #[test]
    fn cancellation_waits_for_the_delayed_frame_close_before_the_next_request() {
        let mut peer = ScriptedPeer::new([
            bytes(b"x}xxch"),
            bytes(b"abort\r\nch> "),
            ReadStep::Delay(Duration::from_millis(225)),
            ReadStep::FrameClose,
        ]);
        let started = Instant::now();

        abort_active_scan(&mut peer, ScanDrain::Records(2), Duration::from_secs(1)).unwrap();
        peer.write_all(b"next\r").unwrap();

        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(peer.writes, b"abort\rnext\r");
    }

    #[test]
    fn cancellation_after_the_frame_waits_only_for_the_abort_reply() {
        let mut peer = ScriptedPeer::new([
            bytes(PROMPT),
            ReadStep::Delay(Duration::from_millis(225)),
            bytes(b"abort\r\nch> "),
        ]);
        let started = Instant::now();

        abort_active_scan(&mut peer, ScanDrain::FrameClosed, Duration::from_secs(1)).unwrap();

        assert!(started.elapsed() >= Duration::from_millis(200));
        assert_eq!(peer.writes, b"abort\r");
    }

    #[test]
    fn cancellation_times_out_when_the_active_frame_never_closes() {
        let mut peer = ScriptedPeer::new([bytes(b"abort\r\nch> ")]);

        assert!(
            abort_active_scan(&mut peer, ScanDrain::Records(1), Duration::from_millis(10),)
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
    }

    #[test]
    fn basic_inputs_expose_only_their_physical_connector_range() {
        assert_eq!(
            frequency_range(Model::Basic, BasicInput::Low),
            (100_000, 350_000_000)
        );
        assert_eq!(
            frequency_range(Model::Basic, BasicInput::High),
            (240_000_000, 959_000_000)
        );
        assert_eq!(
            default_frequency(Model::Basic, BasicInput::High),
            240_000_000
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
                "ultra on",
                "ultra auto",
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
    fn a_span_too_narrow_for_the_point_count_is_clamped() {
        assert_eq!(
            normalize_span(1.0, 959_900_000, DEFAULT_POINTS).unwrap(),
            DEFAULT_POINTS as u64
        );
        let frequencies =
            protocol::scan_frequencies(100_000, 100_000 + DEFAULT_POINTS as u64, DEFAULT_POINTS);
        assert!(frequencies.windows(2).all(|pair| pair[1] > pair[0]));
    }

    #[test]
    fn firmware_frequencies_use_an_endpoint_preserving_display_grid() {
        let measured = protocol::scan_frequencies(100_000_000, 200_000_000, 450);
        assert!(
            measured.windows(2).map(|pair| pair[1] - pair[0]).min()
                != measured.windows(2).map(|pair| pair[1] - pair[0]).max()
        );
        let displayed = display_frequencies(&measured).unwrap();
        assert_eq!(displayed.first(), measured.first());
        assert_eq!(displayed.last(), measured.last());
        let span = displayed.last().unwrap() - displayed.first().unwrap();
        let intervals = displayed.len() as u64 - 1;
        let lower_step = span / intervals;
        let upper_step = span.div_ceil(intervals);
        assert!(displayed.windows(2).all(|pair| {
            let step = pair[1] - pair[0];
            step == lower_step || step == upper_step
        }));
    }

    #[test]
    fn display_grid_rejects_duplicate_firmware_frequencies() {
        assert!(display_frequencies(&[100_000, 100_000, 100_001]).is_err());
        assert!(display_frequencies(&[100_002, 100_001, 100_000]).is_err());
    }

    #[test]
    fn an_edge_shift_becomes_the_next_scan_center() {
        let (start, stop) = centered_window(100_000, 10_000_000, 100_000, 960_000_000);
        let effective_center = window_center(start, stop);
        assert_eq!(effective_center, 5_100_000);
        assert_eq!(
            centered_window(effective_center, 1_000_000, 100_000, 960_000_000),
            (4_600_000, 5_600_000)
        );
    }

    #[test]
    fn direct_sweeps_accept_only_ordered_in_range_limits() {
        let valid = DirectSweepConfig {
            start_hz: 88_000_000,
            stop_hz: 108_000_000,
            generation: 7,
        };
        assert!(validate_direct_sweep(Some(valid), Model::Basic).is_ok());
        assert!(validate_direct_sweep(None, Model::Basic).is_ok());
        assert!(validate_direct_sweep(
            Some(DirectSweepConfig {
                start_hz: valid.stop_hz,
                stop_hz: valid.start_hz,
                ..valid
            }),
            Model::Basic
        )
        .is_err());
        assert!(validate_direct_sweep(
            Some(DirectSweepConfig {
                stop_hz: Model::Basic.maximum_hz() + 1,
                ..valid
            }),
            Model::Basic
        )
        .is_err());
    }

    #[test]
    fn unknown_ultra_uses_the_conservative_zs405_range() {
        assert_eq!(Model::UltraUnknown.maximum_hz(), 6_000_000_000);
    }

    #[test]
    fn startup_forces_every_model_into_input_mode() {
        assert_eq!(
            input_mode_command(Model::Basic, BasicInput::Low),
            "mode low input"
        );
        assert_eq!(
            input_mode_command(Model::Basic, BasicInput::High),
            "mode high input"
        );
        for model in [
            Model::Zs405,
            Model::Zs406,
            Model::Zs407,
            Model::UltraUnknown,
        ] {
            assert_eq!(input_mode_command(model, BasicInput::High), "mode input");
        }
    }

    #[test]
    fn selector_keeps_the_path_and_basic_input_separate() {
        assert_eq!(
            parse_selector("/dev/ttyACM2").unwrap(),
            (Some(PathBuf::from("/dev/ttyACM2")), None)
        );
        assert_eq!(
            parse_selector("/dev/ttyACM2?input=high").unwrap(),
            (Some(PathBuf::from("/dev/ttyACM2")), Some(BasicInput::High))
        );
        assert_eq!(
            parse_selector("?input=high").unwrap(),
            (None, Some(BasicInput::High))
        );
        assert!(parse_selector("/dev/ttyACM2?input=other").is_err());
    }

    #[test]
    fn only_an_explicit_selector_overrides_the_basic_input() {
        let bare = list(Some("/dev/ttyACM2"));
        assert_eq!(bare.len(), 1);
        assert_eq!(bare[0].tiny_sa_input, None);

        let high = list(Some("/dev/ttyACM2?input=high"));
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].tiny_sa_input, Some(BasicInput::High));
    }

    #[test]
    fn basic_capabilities_match_the_selected_input() {
        let low = capabilities(Model::Basic, BasicInput::Low);
        assert_eq!((low.freq_min_hz, low.freq_max_hz), (100_000, 350_000_000));
        let high = capabilities(Model::Basic, BasicInput::High);
        assert_eq!(
            (high.freq_min_hz, high.freq_max_hz),
            (240_000_000, 959_000_000)
        );
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
            let device = TinySaDevice::open(Path::new(&path), BasicInput::Low).unwrap();
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
            assert_eq!(spectrum.target, PowerTraceTarget::Spectrum);
            assert_eq!(spectrum.frequencies_hz.len(), spectrum.levels_dbm.len());
            assert!(!spectrum.frequencies_hz.is_empty());
            device.stop_rx().unwrap();
            assert!(!device.is_streaming());
        }
    }
}
