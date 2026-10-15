//! `vm`: headless CLI for the Linux voice pipeline.
//!
//! Talks to `vm-daemon` today via direct crate calls in-process (Fase 3b
//! moves this onto the WebSocket Control API as its first client).

use clap::{Parser, Subcommand};
use vm_audio::backend::stub::StubBackend;
use vm_audio::{AudioBackend, StreamTopology, DeviceUid};

#[derive(Debug, Parser)]
#[command(name = "vm", about = "Voicemod-Linux headless control (clean-room)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// List audio devices (stub backend until PipeWire lands).
    Devices {
        /// Use the live PipeWire backend.
        #[arg(long)]
        live: bool,
    },
    /// Validate a preset JSON file.
    CheckPreset { path: String },
    /// Print the example preset as JSON.
    ExamplePreset,
    /// Start a stub streaming session (no hardware).
    StartStub,
    /// List supported Control API actions.
    Actions,
    /// List readable evdev keyboards (needs input permissions).
    Keyboards,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Devices { live } => {
            if live {
                match live_devices() {
                    Ok(()) => {}
                    Err(e) => eprintln!("live error: {e}"),
                }
                return;
            }
            let mut backend = StubBackend::new();
            match backend.enumerate() {
                Ok(devs) => {
                    for d in devs {
                        println!(
                            "{:<28} {:<10} {}Hz {}ch{}",
                            d.uid.0,
                            format!("{:?}", d.role).to_lowercase(),
                            d.sample_rate,
                            d.channels,
                            if d.is_bluetooth { " bt" } else { "" },
                        );
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
        }
        Cmd::CheckPreset { path } => match std::fs::read_to_string(&path) {
            Ok(text) => match vm_preset::load(&text) {
                Ok(p) => println!("ok: {} ({} nodes, {} edges)", p.name, p.nodes.len(), p.edges.len()),
                Err(e) => eprintln!("invalid preset: {e}"),
            },
            Err(e) => eprintln!("error: {e}"),
        },
        Cmd::ExamplePreset => {
            println!("{}", serde_json::to_string_pretty(&vm_preset::example_drive()).unwrap());
        }
        Cmd::StartStub => {
            let mut backend = StubBackend::new();
            let _ = backend.enumerate();
            let topo = StreamTopology {
                input: DeviceUid::new("stub:input:0"),
                virtual_out: DeviceUid::new("stub:virtual:vm-mic"),
                monitor: Some(DeviceUid::new("stub:monitor:0")),
            };
            match backend.open(&topo).and_then(|_| backend.start()) {
                Ok(()) => println!("stub streaming started (input → vm-mic + monitor)"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        Cmd::Actions => {
            for a in [
                "selectVoice",
                "toggleVoiceChanger",
                "stopAllSounds",
                "playSound",
                "getAllSoundboard",
                "setVoiceParameter",
                "resetVoiceParameters",
                "setDefaultMicrophone",
                "getAudioDevices",
                "getUserLicense",
                "fadeSound",
                "startAudioStreaming",
                "getAudioStreamStatus",
                "getHealth",
                "addHotkey",
                "removeHotkey",
                "listHotkeys",
                "setMuted",
                "getBitmap",
                "setSoundboardMasterVolume",
                "selectInput",
            ] {
                println!("{a}");
            }
        }
        Cmd::Keyboards => {
            for k in vm_control::HotkeyManager::scan_keyboards() {
                println!("{k}");
            }
        }
    }
}

fn live_devices() -> Result<(), String> {
    use vm_audio::backend::live::PipeWireBackend;
    let mut backend = PipeWireBackend::new().map_err(|e| e.to_string())?;
    let devs = backend.enumerate().map_err(|e| e.to_string())?;
    if devs.is_empty() {
        println!("(no audio devices seen — registry burst still arriving? retry)");
    }
    for d in &devs {
        println!(
            "{:<48} {:<10} {}Hz {}ch{}",
            d.uid.0,
            format!("{:?}", d.role).to_lowercase(),
            d.sample_rate,
            d.channels,
            if d.is_bluetooth { " bt" } else { "" },
        );
    }
    let uid = backend.ensure_virtual_source().map_err(|e| e.to_string())?;
    println!("vm-mic uid: {}", uid.0);
    Ok(())
}
