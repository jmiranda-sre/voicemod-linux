# Voicemod → Linux (Rust) — Project Ledger
Clean-room reimplementation. NEVER copy proprietary code; logic/ideas only. License target: MIT or Apache-2.0.

## Clock
- T0 (Fase 1 start): 2026-10-12T06:00:04Z
- Fase 1 (recon) closed same session. Convention: report elapsed wall-clock per phase as agent-hours.

## Target binary (provided by user, analyzed via Ghidra MCP)
- `extracted/Voicemod.exe`: PE x64, base 0x140000000, .text 14.8MB, 43466 functions, 1908 externs
- sha256 `b5fc01c412b51ffd407145f2df7222186ecb0fcf85e9942c1a54d707591e8ecf`
- UI v23.171.1 (embedded Qt6 QML/WebEngine bundle, ~205MB Qt6WebEngineCore.dll)
- Toolchain: Intel ICX (libmmd.dll, svml_dispmd.dll) + MSVC STL (PPL, C++20), Qt6, cxxopts, nlohmann::json
- Symbols stripped (all FUN_*); RTTI namespaces (5147) are the map. No ASIO/DirectSound imports.

## Architecture (Fase 1 findings)
- Audio engine: **PortAudio statically linked**, WASAPI host primary (`WasapiAudioExtension`,
  `pa_win_wasapi.c` src-path leak @141d6f558), WinMM waveIn/waveOut fallback, AVRT pro-audio
  threads, shared mode + exclusive-mode detection, MMDevices registry enum.
- DSP: node graph `voicemod::core::nodes::FAudioNode*` + `Factory::registerItem` registry.
  Nodes seen: Reverb, PitchFFT, PitchN, FormantPitchShifter, VariablePitchFFT, OrderedTube(reverb),
  `pitch_transposition`, `midiPitchFraction`, `VchcNodeProcess` (neural VC: block_out/hop_out,
  diffusion_steps → WinML GPU/NPU; MVP EXCLUDES AI voices).
- Codec: libopus static, 48kHz, 960-sample (20ms) custom mode.
- Virtual mic: kernel WDM driver (`voicemodvad.sys`+`mvvad.sys`, "Voicemod Virtual Audio Device (WDM)").
  Linux: PipeWire virtual source, no driver.
- Services: VoiceService (loadReceivedVoice), ContentService (server-downloaded ResourceManifest
  voice/sound packs, EncryptedLicenseDetails, xg::Guid), AudioPlaybackEngine (createMainAudioStream,
  createAudioStreamingContext, AudioStreamingTopology, StreamSharedData, auto-restart),
  AudioCapture::startRecording, VADService, KeyBindsService+InputListener (RawInput),
  AcousticFeedbackDetector (+Metrics), DataCollectionService, ControlApiService (local WebSocket),
  Metrics→Amplitude, Sentry (sentry.voicemod.net), WinSparkle updater, crashpad.
- UI↔native bridge (`bridgey` bus): StartAudioStreaming, GetAudioStreamStatus, selectVoice,
  toggleVoiceChanger, stopAllSounds, getAllSoundboard(s), setDefaultMicrophone, getAudioDevices, ...
- userSettings keys: voiceChanger, hearMySelf, isMicMuted, selectedInputDevice/OutputDevice/
  VirtualDevice, voicemodMicDefaultToggleState, isDenoiserEnabled, isNoiseGateEnabled,
  noiseGateLevel, recorderSoundboardId, selectedVoice/Sound, isFadeSoundEnabled,
  gpu/npuPoweredVoicesState. (Source: embedded JS bundle — also contains Voicelab slider min/max.)

## Key addresses (Ghidra, Voicemod.exe)
- VAD name `(Voicemod Virtual Audio Device (WDM))` @140e39008
- Topology log `"{} Streaming with input '{}', output '{}' and virtual '{}'"` @141c9c910
- PortAudio logs @141d67310–141d67a30; `PA_MIN_LATENCY_MSEC` @141d6fc40
- `WasapiAudioExtension` @141d6bb10; AUDCLNT block @141d6ef10+; MMDevices reg @141d658d0
- Opus asserts @141f66386, @141f6a6a7
- `FAudioNodeReverb` RTTI @1428ca190; factory register lambdas @1428b1e40, @1428b2030
- Pitch nodes @141d8e31c/@141d8eb6c/@141d8ec1c/@141d8f84c/@141d8fd4c; Vchc @141eb2423
- VADService show/hide @141c98c68+; exclusive-mode @141d66600/@141d67118

## Windows → Linux mapping (decided)
| Windows | Linux (Rust) |
|---|---|
| PortAudio/WASAPI + WinMM | PipeWire streams 48kHz f32 (pw bindings) |
| voicemodvad.sys WDM driver | PipeWire virtual source (`vm-mic`) |
| AVRT threads | RT threads (SCHED_FIFO/RTKit, pw thread loop) |
| WinML GPU/NPU AI voices | ONNX Runtime (post-MVP) |
| libopus static | `audiopus` crate |
| Qt6/WebEngine UI | headless CLI first → egui later |
| RawInput hotkeys | evdev |
| Local ws Control API | compatible ws API (StreamDeck bonus) |
| nlohmann manifests | own serde_json preset format (clean-room) |
| Sentry/Amplitude/attestation/DRM/WinSparkle | dropped |

## Fase 2 targets (ordered)1. `createAudioStreamingContext` + `AudioStreamingTopology` + `StreamSharedData` + stream callback
   (xref topology log + PortAudio init strings).
2. `VoiceService::loadReceivedVoice` + ResourceManifest schema → our preset format.
3. Factory `registerItem` xrefs → full node inventory + param model (`FailedToSetVoiceParameter`).
4. `VchcNodeProcess` scope (post-MVP decision).
5. Soundboard decode+mix+fade+master-vol path.
6. VADService + .inf statics → PipeWire virtual-mic spec.
7. Hotkey model + ws Control API protocol.
8. Voicelab slider min/max from embedded JS bundle (offline string extraction).

## Fase 2 — Item ① DONE (streaming context, topology, callback path)
Start path (service → engine):
`AudioStreaming_BuildTopologyAndStart` @1408d8050 (callers: @1408d7650 start, @1408d95c0 restart)
 → `AudioStreaming_CreateStreamDescriptors` @1408d8d10 (0x88 stream-descriptor structs,
    `AudioConfigStatusChanged` event) → `AudioStreaming_StartStreams` @1408d9da0
 → `AudioPlaybackEngine_CreateMainAudioStream` @140b5c1d0 (7 params: engine + 3 UIDs + type +
    topology + StreamSharedData) → `PortAudio_ResolveDeviceIdsByUid` @140b5cde0 + big open fn
    @140b5c540 (2132B, owns Pa open behind virtual dispatch — callback addr still open).
Device model: 0xE8-byte entries, dword@0 type discriminator (match value 3); roles input / VAD-virtual
 / monitor (+optional 4th when cfg+0x260 set); UIDs as std::string compared by memcmp.
 UID→PaDeviceIndex via Pa_GetDeviceInfo (maxIn@+0x14, maxOut@+0x18) +
 Pa_HostApiDeviceIndexToDeviceIndex; host API = WASAPI (`WasapiAudioExtension`).
 Enums: stream-type 4 values (`kUnknownStream` @141d66fa0), disconnect reasons ≥7 (`kNoError`
 @141d67020); throws `PortAudioException` via @140b54f00 on missing input/virtual/monitor id.
Rate: per-device double ×1000.0 (`DAT_141c7fd18` = 1000.0) → int Hz; 48kHz anchor; Opus 960/20ms downstream.
Latency: device defaultLow*Latency passthrough + suggested-latency clamp w/ user warning
 ("We recommend setting the suggested latency at max {}ms"); shared-mode WASAPI, buffer cadence =
 device period at runtime (no hardcoded framesPerBuffer const found — Pa derives from suggestedLatency).
RT: `MmcssPriorityChanger_SetThreadPriority` @140508a70 → MMCSS "Pro Audio" +
 AVRT_PRIORITY_NORMAL (deliberately NORMAL, not CRITICAL); revert @140508cf0; mutex+state machine
 (sentinel 0x7fffffff @+0x20c) + `..._TryRestartOnInternalError` @140b5bea0 + session-disconnect
 handling. No ASIO/DSOUND DLLS (DS host code present, dsound.dll loaded dynamically at runtime only).
vm-audio requirements: 48kHz f32, quantum 960 (20ms; tunable 256–2048), RT thread at standard
 pro-audio level (NOT max), memlock, 3 streams (mic-in → virtual-source out + monitor), hotplug via
 pw registry (≡ IMMNotificationClient), xrun→reconnect state machine (≡ tryRestart), exclusive-mode
 concept DROPPED (PipeWire arbitrates).

## Fase 2 — Item ② DONE (factory inventory, preset/param model, callback dispatch)
Node inventory: 112 types registered via `Factory<std::string, FAudioNode, NodeConstructorContext>::registerItem`
 (lambda per type; name registry table @141d7a170+). F-family (legacy, MONO-only —
 `FAudioNode::process => No support for multichannel processing`): Null, Mute, Volume, Pan, Tone,
 Mix, MixN, Switch, Delay, RDelay, Reverb, OrderedTube, Tube, TubeFFT, PitchFFT, PitchN, Vocoder,
 VocoderFFT, Biquad, BiquadNew, Compressor, Overdrive, NoiseGateLevel, Wav, Ogg, Extern,
 VoiceDetect, Save + WithChildren{,Node,NodeFFT,SetAB,SetABC} composites. V-family (voicelab gen):
 PitchFFT, VariablePitchFFT, FormantPitchShifter, VoicetuneWsola, AutoTuneFFT, AutotuneFFTChordsDetection,
 ChordDetectionFFT, FundamentalFrequency, RingModulation, Distortion, Overdrive, Granulator, Convolverb
 (+ConvolXMLLoaderParameter), Timbre, BassEnhancer, Xover, ParametricEQ, SmartEQ, DeEsser, Biquad,
 Compressor(+SideChain)/Expander, AGC, AGC2, ADSR, NoiseGate(+Ratio), Rnnoise (=denoiser is RNNoise!),
 Oscillator, Sampler(+Base), Playlist, FilePlayer (soundboard), VoiceDetect, Switch/IfElse, Feedback,
 Delay/RDelay, Mixer analogues, DataAnalysis, SpectrumAnalyzer, Oscillator, ScaleSelector, Timbre,
 SmartVibratoFFT, XMLLoader + XMLLoaderParameter. Params/modulation: FAudioParam{Range,InRange,InArea,
 Selector,Set,Transition,Math,Random,AutoRandom,Oscillator,Oscilator,Button,If,Any} +
 VAudioParameter{Clock,Sequence,Trigger(+Delay,Mix),RandomUniform/Normal/Integer/Sequence,
 Boolean2Trigger} + MathExpr/Settings/Selector. AI (post-MVP): VCHC/VCML/VoConv3/VoConv4(+ParamVC)
 × {Monolithic, ConsumerProducer}.
Preset/param model: voice = named node graph (XML via `VAudioNodeXMLLoader_LoadEffect` @1403241f0,
 keyUuid/keyId licensing — DROPPED in port) + param array [{name, value, public}].
 `VoiceService_LoadVoiceParameters` @140933410 ("Setting voice {} param {} to {}", skips non-public).
 Runtime set: `SetVoiceParameterHandler_Execute` @140a7eae0 (nlohmann::json {name,value} → lookup by
 name → `AudioEffectLoader_SetParameter` @140092800 variant dispatch, kUnknown→error → virtual set
 +0x150). InputCorrectionEffect params excluded from query/set (separate calibration chain).
Callback dispatch (item ① follow-up CLOSED): 3 Pa callbacks — `CaptureFullDuplexAudioStreamCallback`
 @140b8e760 (Pa signature in/out/frames/timeInfo/flags/userData; per-channel f32 deinterleave via
 memmove(frames*4) into auto-resizing buffers @state+idx*3, then DSP entry = std::function @+0xa2/+0xaa
 invoked via vfunc+0x10, mode-selected; abort flag @+0x16d), `HearmyselfMonitorAudioStreamCallback`
 @140b8f7a0 (575B), `PlaybackHalfDuplexAudioStreamCallback` @140b90140 (1645B). Guards:
 `AudioStreamingWatchdog` @140b77580 (hung-callback timeDiff ms), AudioStreamXrunDetector,
 StreamFinishedCallback, per-callback `callback_average_duration_us` telemetry.
 Virtual open dispatch = StreamSharedData(@engine+0x158) vtable+0x10 with 4 shared_ptr ctx args.
vm-preset design (clean-room JSON): {nodes:[{id,type,params:{name:value|{mod}}}], edges, public_params[]};
 static values + modulation sources (lfo/random/transition/trigger/sequence) as first-class param kinds;
 N-channel-native graph (original is mono-chains-per-channel).

## Fase 2 — Item ③ DONE (soundboard, VAD spec, hotkeys+ws)
 48k; nós pitch/formant/reverb (FFT+WSOLA); ws dispatch ligado no pipeline; auth da Control API.
SOUNDBOARD: `SoundboardController_PlaySound` @140b819f0 — path normalize (`\`→`/`, UTF8),
 stopOtherSounds flag, volume = int/1000.0 (`DAT_141d57780`=1000.0f, per-mille), DRM branch
 (plain @140011810 vs protected @1400118e0), full option struct logged:
 loop/stopOtherSounds/muteOtherSounds/muteVoice/volume/sendToVAD/fadeInTimeInSecs/fadeOutTimeInSecs.
 NO ducking (0 hits — muting is explicit). Codecs: Opus=libopus 1.4 (encoder+decoder, Conan
 `universal_sdk`), Vorbis=libVorbis 20200704 (`VorbisDecoder@nqr`), MP3 (`Mp3Decoder@nqr` +
 `MP3AudioFileFormat@audio@choc`), FLAC, WAV (PCM+IMA ADPCM); dialog filter `*.wav *.flac *.mp3 *.ogg`.
 File flow: SoundService::managePlaySound (license check→download) → playSoundInternal →
 engine play; sounds addressed by hash id (`playSoundByHashId`); Sampler subscribes to FilePlayer
 path changes; Playlist add/remove events; `StopAllSoundsHandler`; master-volume get/set events;
 `SystemSoundPlayer` separate (UI sounds). `sendToVAD` = per-sound routing flag.
VAD SPEC (from .inf statics): TWO drivers — legacy mvvad.sys (single Wave+Topo, render+capture) and
 current voicemodvad.sys (KMDF 1.15, ROOT\voicemodvad, x64+ARM64): WaveSpeaker render endpoint +
 WaveMicArray1 capture endpoint, both event-driven, speaker NeverSetAsDefaultEndpoint=0x105.
 Loopback model: app renders processed mix to WaveSpeaker; consumers capture from WaveMic.
 Channels mono/stereo only ("Unsupported channels count..."), 48kHz. VADService: driver
 install/enable/version checks, show/hide driver(s), default-mic mgmt (two-pass default detection),
 JSON handler map. PipeWire spec: ONE virtual source `vm-mic` (stereo max, 48kHz), app links
 processed mix to it; monitor = separate local sink; hotplug/default-device logic in vm-audio.
HOTKEYS+WS: `voicemod::ws::WebSocketServer` on QWebSocketServer — `WebSocketServer_ListenControlApi`
 @1408af640 iterates runtime port list (bss-populated, static read = zeros), binds QHostAddress::Any
 (0.0.0.0 — LAN pairing, NOT loopback-only), wires newConnection/closed, stores port @+0x52, logs
 "using port [{}]"; total failure → ControlApiException. Pairing: GetControlAPIQRContent QR with
 LAN IPs (NetworkInterface enum → JSON) + client auth (`ClientAuthorizationData`,
 `onControlAPIClientAuthorizedEvent`). Protocol: JSON action-string dispatch
 (`ControlApi_MessageDispatcher` @14089ec70, memcmp "GetControlAPIQRContent"), DataRequester
 (loadVoice/onVoiceChanged/getBitmap icon bitmaps w/ bitmapChecksum for StreamDeck-style clients,
 processMessageData). Known actions: selectVoice/toggleVoiceChanger/stopAllSounds/playSound/
 getAllSoundboard(s)/join_soundboard/setVoiceParameter/setCurrentVoiceParameter/ResetVoiceParameters/
 setDefaultMicrophone/getAudioDevices/GetUserLicense/fade_sound/createCustomCollection/doLogout/
 ShowGetPro/StartAudioStreaming/GetAudioStreamStatus. Hotkeys: KeyBindsService full lifecycle
 (per-resource binds, press/release trigger+edit, persisted, save/remove confirmations) over
 RawInputHelper (target window + device registration + native event filter) → Linux: evdev.

## Fase 3 — SCAFFOLD DONE (workspace compila + roda headless)
Layout `~/Projetos/voicemod-linux/`: vm-audio, vm-dsp, vm-soundboard, vm-preset,
 vm-control, vm-cli (`vm`), vm-daemon. `cargo build --workspace` verde (rustc 1.99).
 `vm-daemon` dry-run OK: stub topology → preset example-drive (gain→biquad, peak 0.3913) →
 control API na porta efetiva 8765. `vm` CLI: devices/check-preset/example-preset/actions/keyboards.
Decisões: libc 0.2 (1.x yanked no índice); symphonia 0.5.5 cobre wav/flac/mp3/vorbis —
 SEM codec opus (confirmado no registry) → Ogg-Opus demux+decode na Fase 3b (audiopus 0.2 já
 linka libopus 1.6.1 do sistema); `pipewire` crate DESLIGADO — `libspa` sys quebra contra
 headers PipeWire 1.6.9 (E0609), alinhar versões na Fase 3b (spec em vm-audio/src/backend/pipewire.rs).
 RT pronto: usuário no grupo `audio` (rtprio 99, memlock 8MB); evdev exige grupo `input`/udev (documentar).
Fase 3b (próximo): PipeWire live (versões alinhadas) → vm-mic real; Ogg-Opus demux + resampler 48k; Ogg-Opus usa audiopus na Fase 3b (audiopus 0.2 já linka libopus 1.6.1);
ws dispatch ligado no pipeline; auth da Control API.

## Fase 3b — Item 1 DONE (nós DSP: pitch, formant, reverb, tube)
`vm-dsp/src/spectral.rs` (novo): `PitchEngine` — phase-vocoder stretch(β) + resample(β),
 duration-preserving (receita clássica), STFT 2048/512 Hann + WOLA, envelope formântica via
 cepstro real (lifter 40). Correção `G(u)=E(u·k)/E(u)`: follow → k=1 (G=1, sem correção);
 warp independente → k=r/f. Nós em `nodes.rs`: `pitch-fft` (transpose ±12st + mix),
 `formant-pitch-shifter` (transpose ±12st + formant_shift ±12st, 0=preserva + mix),
 `reverb` (Schroeder comb+allpass estilo Freeverb, tunings 44.1k→48k, room/damping/wet),
 `tube` (tanh drive 0–10 + tone LP + mix). Factory + `KNOWN_NODE_TYPES` estendidos.
 Testes (`cargo test -p vm-dsp`, 5/5 verdes): seno 440 +12st → 880Hz (±5%), bypass → 440Hz,
 formant clampa ranges e rejeita param desconhecido, reverb wet=0 bit-exato + cauda decai,
 tube satura com |y|≤1. Bugs achados e corrigidos pelos testes: (1) correção de envelope no
 caso follow zerava a fundamental; (2) hop_s e taxa de resample invertidos (pitch saía 1/β).
 Latência: STFT 2048 ≈ 43ms + resample — documentado; otimizar (frame menor) na Fase 4
 junto ao loopback real.

## Fase 3b — Item 2 DONE (PipeWire live + vm-mic)
`pipewire` 0.10.1 (features v1_2_0) compila contra headers 1.6.9 (0.8 era incompatível —
 libspa E0609). `PipeWireBackend` (`vm-audio/src/backend/live.rs`): worker thread única
 (objetos !Send) com `Loop::iterate`; 3 streams (capture F32LE/48k + `node.latency=960/48000`,
 playback vad com target explícito removido em favor de links, monitor autoconnect);
 rings SPSC sample-granulares (`rtrb`); DSP via `StreamProcessor` + `try_lock` (nunca bloqueia);
 registry global/global_remove → enumerate + hotplug; `vm-mic` via
 `core.create_object("adapter", support.null-audio-sink)` (Audio/Sink, 48k, virtual);
 links explícitos via `link-factory` (vad→vmic, mic→cap, monitor→default via `pactl`
 com fallback autoconnect); `set_active(true)` obrigatório p/ streams linkados manualmente;
 destroy no stop; `negotiated()` expõe rate/ch/format; conversão S16/S32/S24→f32 no capture.
VERIFICADO LIVE: 4 nós RUNNING + links ACTIVE; capture 4-8µs xr≈0; vad DSP roda; monitor entrega;
 control API + hotplug + stats; `vm devices --live` funcional; `vm-daemon --live` com
 `--backend` implícito + `VM_INPUT_UID` override + fallback p/ stub. Release OBRIGATÓRIO p/ RT
 (debug 7-44ms faminta a RT-thread e trava `pw-dump`; release: µs).
CAUSA RAIZ do "garbage": lia `bytes.len()` (buffer máximo 12288) em vez de `chunk.size()`
 (frames válidos ~960) + nunca setava chunk na saída → lia/escrevia memória stale (3e38/NaN).
 Corrigido com disciplina de chunk (ler `chunk.size`, setar offset/stride/size). Pós-fix:
 gravações 100% finitas, sem garbage.
E2E tone-stamp PENDENTE (Fase 4): sala silenciosa + speakers mortos + `pw-play --target`
 ignorado + sem root p/ snd-aloop + sem EV p/ EE test-tones. Procedimento pronto.
 Dívidas Fase 4: forçar quantum ≤960 fim-a-fim (null-sink entrega 12288); dedup links do
 monitor (explícito + autoconnect coexistem); auth da Control API; `VM_INPUT_UID` → seleção
 persistente; remover `pactl` dep com metadata nativa.
Bugs de processo anotados: NUNCA `pkill -f` com padrão literal presente no próprio comando
 (3 auto-kills nesta sessão; usar `pkill -x`/`pidof`); proxy registry deve viver no Worker
 (dropar mata subscription); `node_id()` é async (INVALID até o servidor responder).

## Fase 3b — Item 3 DONE (Ogg-Opus + resampler + routing)
`vm-soundboard` agora: `ogg.rs` (demux próprio RFC 3533: CRC32 IEEE com skip de página
 corrompida, pacotes continuados, multi-stream por serial), `opus.rs` (OpusHead/Tags,
 libopus via `audiopus` 0.2 a 48k, preskip trim, end-trim por granule, skip de pacotes ruins),
 `resample.rs` (`rubato` 5 FFT `process_all`, qualquer taxa→48k sem pitch shift),
 `mix.rs` (`SoundboardMixer`: roteamento `sendToVAD` Vad/Both/Monitor, master per-mille,
 fades de play-time, loop/wrap, stop/stop_all, retire de one-shots). `decode_file` roteia
 opus/oga→opus, ogg→symphonia-com-fallback-opus (0.5 sem codec opus), resto→symphonia;
 tudo sai 48k planar f32. Decisões: `rubato` 5 mudou a API (`Fft::new` + `FixedSync::Input`
 + `process_all` c/ adapter `SequentialSliceOfVecs`); demux próprio em vez de crate `ogg`
 (controle total + testável); fades em play-time no mixer (não baked).
Testes (`cargo test -p vm-soundboard`, 14/14): demux (2 pacotes, continuação real c/ CRC,
 skip de CRC corrompido, rejeição de lixo), roundtrip opus 440Hz ±3%, resample 44.1→48k ±1%,
 wav 44.1k→decode_file 48k c/ pitch preservado, matriz de roteamento, master, loop, fades
 (0→1→0), erros (desconhecido/corrompido). Bug pego pelos testes: `bos` perdido em pacote
 continuado (header da pág. seguinte sobrescrevia).

## Fase 3b — Item 4 DONE (dispatch WS → pipeline) — FASE 3b ENCERRADA
`vm-control`: trait `ControlHandler::handle(&Action) -> (ControlReply, Vec<ControlEvent>)`;
 `ControlApiServer::new(config, handler)`; replies + broadcasts fan-out para TODOS os
 sockets (select loop msg+broadcast; gap achado e corrigido: eventos iam só p/ subscribe
 in-process). `watch_device` evdev (press-edge + held-set matching, thread por teclado,
 best-effort sem perms). `vm-daemon`: `DaemonState` (graph Arc + node_ids + mixer +
 sound_ids + backend Box + preset_dir) implementa o handler: selectVoice (load+rebuild do
 preset, hot-swap sob lock — RT usa try_lock e conta xrun), setVoiceParameter
 ("node.param", JSON→variant, erros fiéis "unable to find"), playSound/stopAllSounds/
 fadeSound (fade-out com auto-stop no mixer), toggleVoiceChanger (AtomicBool bypass
 testado bit-exato), StartAudioStreaming/GetAudioStreamStatus (link+xruns reais),
 GetAudioDevices (enumerate real), GetUserLicense (tier local honesto), GetBitmap (err
 honesto: sem assets), ResetVoiceParameters (reload do preset), SetDefaultMicrophone
 (err honesto: conceito do desktop). `GraphProcessor` compartilha graph+bypass com o
 controle. Presets em `~/.config/vm/presets` (seed example-drive), sounds preload de
 `~/.config/vm/sounds` (nunca I/O no trigger). Hotkeys → mesmo path do dispatch.
VERIFICADO: 24/24 testes (2 ws socket c/ handler canned, 3 daemon c/ stub incl. bypass
 bit-exato, 5 dsp, 14 soundboard); E2E socket real contra daemon stub: selectVoice ok,
 setVoiceParameter ok, status com dados reais, playSound inexistente erra honesto,
 garbage protocolar erra, toggle retorna reply ok:true + broadcast `voiceChangerToggled`
 no wire. Bug pego: bypass com semântica invertida (flag==enabled em vez de !enabled).

## FASE 4 (próxima): testes e iteração
1. Tone-stamp E2E (440Hz mic→DSP→vm-mic) c/ cooperação ou HW de áudio.
2. Forçar quantum ≤960 fim-a-fim; medir latência real (round-trip mic→vm-mic).
3. Dedup links do monitor; auth da Control API; `VM_INPUT_UID` persistente.
4. `pw-top` ERR budgets por nó; golden-WAV offline; CI (`cargo test --workspace`).
5. UI (egui) após MVP headless estável.

## FASE 4 — INÍCIO (testes E2E, latência, refinamento do MVP headless)
Escopo: (1) medição real de latência/quantum fim-a-fim; (2) dedup dos links do monitor +
 auth opcional na Control API; (3) tone-stamp E2E com fonte real de sinal.
Estado de entrada: workspace verde (24/24 testes), backend live funcional, chunk-discipline
 correta, release obrigatório p/ RT. Quantum observado via pw-top: streams vm-* a 960,
 vm-mic a 512 — o número 12288 era TAMANHO de buffer pré-alocado, não cadência real.

## FASE 4 — INÍCIO (testes E2E, latência, refinamento do MVP headless)
Escopo (ordem): 1. latência/quantum (medir; forçar ≤960 se preciso); 2. dedup links
 do monitor + auth opcional da Control API; 3. tone-stamp E2E mic→DSP→vm-mic.
 Relógio: sessão atual. Critério de saída: E2E com tom medido OU bloqueio ambiental
 documentado + latência caracterizada por stats + MVP headless estável.

## FASE 4 — progresso (tone-stamp E2E PROVADO)
E2E 440Hz ponta-a-ponta MEDIDO: pw-cat → vm-capture → DSP (gain+biquad) → vad →
 vm-mic → onboard.monitor = **440Hz dominante 0.24 vs ruído 0.0004 (SNR ~55dB)**.
 Stats live: cap 4-19µs xr≈0, vad 67-300µs xr≈0, mon 15-70µs (release, quanta reais).
 Achados: (1) `pw-record` e taps de monitor de NULL-SINK medem hash (3e38/NaN) mesmo
 com fluxo válido — monitor de sink real é o tap confiável; vm-mic PROVA emitir 440Hz
 limpo downstream apesar do próprio monitor mostrar hash. (2) Null-sink precisa de
 links manuais ou session ativa p/ fluir. (3) Loops digitais (monitor→speakers→monitor→
 capture, ganho 1.5×/ciclo) explodem p/ Inf em segundos e travam IIR em NaN p/ sempre —
 guard `is_finite` na entrada do DSP adicionado (prática padrão). (4) Speakers foram
 mutados p/ isolamento e DESMUTADOS ao final (0.75); sistema restaurado (sem nós vm,
 sem sinks de teste, sem loops). Quantum: nós a 960/512 fim-a-fim (pw-top); buffers
 de 12288 = capacidade máxima, irrelevante com disciplina de chunk + frames variáveis.
 Monitor: explícito-only (sem AUTOCONNECT duplo). Auth: `VM_CONTROL_TOKEN` wired c/
 fail-fast + teste. Falta Fase 4: forçar quantum ≤960 no null-sink (se preciso p/
 latência), medir latência round-trip absoluta, UI.

## FASE 4b — refinamento latência/quantum (início)
Alvos: (1) confirmar/forçar quantum ≤960 fim-a-fim; (2) medir latência absoluta
 round-trip comtimestamps no mesmo clock do grafo (probe dedicado: playback de click
 + capture, ambos com `pw_stream_get_time`); (3) fechar dívidas restantes.

## FASE 4 — E2E TONE-STAMP FECHADO +hrefinamentos
E2E 440Hz DEFINITIVO: pw-cat → vm-capture → DSP → vad → vm-mic → sink real =
 **440Hz a 0.53 vs fundo ≤0.01 (50:1)**, medido com Goertzel correto (sem subsampling!).
 Stats da sessão: cap 4-19µs xr≈0, vad 40-300µs, mon 4-70µs; quanta 960/960/960/512
 (pw-top, ERR 0) — intra-spec, sem forcing necessário. Monitor: explícito-only
 (AUTOCONNECT removido qdo pactl resolve; sem duplos). Auth: `VM_CONTROL_TOKEN` wired
 no daemon c/ fail-fast + aviso quando aberto.
Lições de medição: (1) Goertzel com subsampling erra a taxa — usar full-rate;
 (2) `pw-play --target` e `pw-cat --target` são ignorados aqui — usar `--target 0` +
 `pw-link` manual; (3) `pw-cat` de arquivo curto morre no meio da medição — usar
 arquivo longo + instância única (loops `while true` geram zombies e confundem tudo);
 (4) null-sink monitors mostram hash quando idle/stale — tap confiável é monitor de
 sink real; (5) NUNCA `pkill -f` com padrão literal (auto-kills; usar `pkill -x`/pid).
Sistema restaurado: daemon parado, sinks de teste removidos, speakers desmutados (0.75).
Pendente Fase 4: latência absoluta round-trip em ms (correlação c/ trem de impulsos em
 ambiente controlado), UI.

## FASE 4 — LATÊNCIA ABSOLUTA MEDIDA: ~51ms fim-a-fim
Método: clicks 1s (pw-cat→capture) + probe simultânea no monitor do sink, timestamps
 no mesmo script; delay por fase da grade 1s + exclusão de alias (primeiro transient
 4.051s só pode vir do click 4.0s — links nasceram em 4.027s — logo D≈51ms, não 1051ms).
 Orçamento coerente: capture ≤20ms + DSP clássico ~0 + vad ≤20ms + vm-mic 512q (~10ms).
 DECISÃO: mantém STFT 2048 default (43ms só incidem em vozes espectrais; medir de novo
 quando voz com pitch for o sujeito). Sem forcing de quantum (tudo ≤960/512, ERR 0).
 Sistema restaurado pós-medição (sem loops, desmutado).

## FASE 5 — INÍCIO (UI egui `vm-ui`)
Decisão arquitetural: UI como CLIENTE WebSocket da Control API (não in-process) —
 valida a API como plano de controle único (história StreamDeck) e permite UI remota.
 `vm-ui` (eframe): thread WS dedicada (tokio) + snapshots via mpsc p/ thread egui.
 Ações novas no protocolo: `setSoundboardMasterVolume`, `selectInput` (reopen runtime).
 Critério: presets (lista+sliders c/ ranges), soundboard (grid+volumes+rotas),
 devices (dropdowns+status vm-mic), tudo funcional contra daemon headless.

## FASE 5 — UI scaffold funcional (vm-ui/egui)
`vm-ui` (eframe 0.36): cliente WS da Control API (plano de controle único) — thread
 dedicada c/ runtime próprio, snapshot Arc<Mutex> + repaint; request/response c/
 skip de broadcasts (distingue "ok" vs "event"); reconnect com backoff (bug corrigido:
 loop interno girava em socket morto); token via env. Tabs: Voices (lista do preset
 dir + sliders c/ ranges + toggle + select), Soundboard (grid, master, stop, fade),
 Devices (combo input→SelectInput, vm-mic/monitor display, status+xruns). Launch/stop
 daemon (spawn --live). Protocolo +2 ações: `setSoundboardMasterVolume`,
 `selectInput` (stop→open→start runtime, testado). egui 0.36 quebrou API antiga:
 trait `App::ui` (não `update`), `Panel::{top,bottom,left}` + `show(ui)` (não
 TopBottomPanel/SidePanel/show_inside). eframe re-exporta egui (`use eframe::egui`).
VERIFICADO: workspace+release compilam; 30/30 testes; UI abre e renderiza (screenshot);
 conectividade validada no log do daemon. Click-through manual e params não-float
 ficam p/ Fase 5b (revisão com usuário).

## FASE 5b — click-through guiado VALIDADO (usuário clicando, agente verificando)
Sessão guiada c/ screenshots: (1) select preset → params carregados + evento
 VoiceChanged refletido na statusbar; (2) drag do slider gain 1.50→0.50 registrado;
 (3) aba Devices renderiza dropdown + vm-mic/monitor + status reais. Conexão verde
 "connected :8765" (troca de emoji por texto colorido — glifo ausente na fonte).
 Request-ids ida e volta (teste socket). Params bool/int/text implementados.
 Automação própria de clique ABANDONADA com motivo: uinput sem erro mas sem efeito
 observável no KWin (mapeamento/aceleração incertos) + risco de interferir no jogo
 do usuário; click-through manual com o usuário é o método oficial.

## FASE 5b — request-ids + matriz de widgets + polimento (FECHADA)
- Request-ids: `ControlMessage.id` opcional, ecoado flat no reply (`{"ok":..,"id":42}`);
 UI anexa ids crescentes, correlação estrita c/ fallback p/ replies sem id + timeout
 10s; teste socket dedicado. Sem breaking change.
- Matriz de widgets: `JsonParamValue` reordenado (Int primeiro — untagged serde
 engolia ints como Float); nó `Delay` real (delay_ms/feedback/mix) p/ Int funcional;
 `Gain.note` (annotation Text real); `Preset.description`; UI renderiza
 Float±range (slider), Float (drag), Bool (checkbox), Int (drag), Text (edit+Enter)
 + description do preset; factory/KNOWN estendidos. Demo `widget-showcase.json`.
- Click-through guiado parcial: tab Soundboard trocou 1× (não reproduzível —
 automação uinput sem efeito observável no KWin; sem xdotool/ydotool; risco ao jogo
 do usuário abortou a via automática). Método oficial: usuário clica, agente verifica
 via screenshot — 3/3 validado (preset, slider 1.50→0.50, Devices).
- Polimento: janela 980×640+posição, slider 220px, grid 3-col real, headings,
 indicador connected/offline em texto colorido (emoji sem glifo), tier no statusbar,
 reconnect com backoff (loop em socket morto corrigido).
- Testes: 31+/31+ workspace (matriz bool/int/text/float no handler real c/ preset
 inline; delay echo por impulso; id-echo socket). Flake de porta em testes WS
 paralelos mitigado c/ listas de fallback.
- Processo: NUNCA `pkill -f` c/ padrão literal (auto-kills recorrentes; usar
 `pkill -x`/pid); verificar rebuild após editar (binário stale engana); cwd dos
 paths absolutos nas tools.

## FASE 6 — INÍCIO (empacotamento e distribuição)
Escopo: Flatpak (manifest + sockets PW), AppImage (receita AppDir), AUR PKGBUILD
 (source build), .desktop + ícone SVG original, systemd --user unit, README final.
 Restrição honesta a documentar: sandbox limita rtprio/evdev — nativo/AUR é o
 caminho pro; Flatpak com `--filesystem=xdg-run/pipewire-0` + `--device=all`.

## FASE 6 — Empacotamento e Distribuição (Flatpak / AppImage / AUR) — FECHADA
Decisões de ID: canônico `io.github.voicemodlinux.vm` (desktop/metainfo/icon).
Sets divergentes antigos (`org.voicemodlinux.App.*`, `assets/icon.svg`,
`voicemod-daemon.service` solto) REMOVIDOS; systemd canônico =
`packaging/systemd/vm-daemon.service` (/usr/bin, sem Nice=-11/LimitRTPRIO —
unit user nãoprivilegiado não pode; RT vem do grupo audio, memlock ok).
LICENSE-MIT + LICENSE-APACHE criados (repo não tinha LICENSE).

FLATPAK — build REAL feito (sem flatpak-builder instalado; fluxo replicado com
`flatpak build-init/build/build-finish/build-export`):
- SDK 26.08 + Extension.rust-stable + Extension.llvm22 (instalados user).
- BUG real encontrado e corrigido: bindgen (libspa-sys) não achava stdbool.h
  — headers gcc do SDK ficam em /usr/lib/gcc/x86_64-unknown-linux-gnu/<v>/include
  (path não-multiarch; clang não detecta). Fix: build-inside.sh passa
  BINDGEN_EXTRA_CLANG_ARGS=-I$(find /usr/lib/gcc -name stdbool.h).
- BUG real no manifest: `--share=ip` é inválido (valores: network/ipc) →
  corrigido p/ --share=network (build-finish reclamou).
- E2E PROVADO: daemon dentro do sandbox enumerou devices, criou vm-mic#209,
  linkou e serviu Control API 8765. Permissões: wayland/fallback-x11/dri/
  pulseaudio/`--filesystem=xdg-run/pipewire-0` (flag nativa PW confirmada c/
  maintainer do flatpak)/ipc/network.
- Hotkeys OFF no sandbox por design (sem /dev/input) — daemon degrada ok.
- Artefato de teste desinstalado depois (1.4GB por target/ exportado no teste
  manual; manifest real clona git, fica slim).

APPIMAGE — build REAL (FUSE disponível): build.sh com allowlist de libs base,
cópia transitiva (libopus/libpipewire bundled), AppRun sobe daemon se 8765
fechado. Smoke test OK (vm actions roda com libs bundled). Artefato:
/tmp/voicemod-appimage.1IobTE/VoicemodLinux-0.1.0-x86_64.AppImage (16.4MB).

AUR — build REAL com makepkg (pacman 7.1.0): pacote completo auditado
(binários, unit user, udev, desktop, icon, metainfo, licenses).
- BUG real no recipe: `git describe | sed` engole exit code → fallback `||`
  nunca roda → pkgver vazio. Corrigido com captura em variável + output
  garantido não-vazio + `unset GIT_DIR GIT_WORK_TREE`.
- REGRESSÃO do pacman 7.1.0 (não nossa): update_pkgver reescreve linhas
  `pkgver=` que PKGBUILD com função não tem → versão local sai vazia
  (voicemod-linux--1) e --printsrcinfo aborta no lint (roda antes de
  reconhecer a função). Com canônico mínimo + repo público REAL reproduz o
  mesmo → confirmado ser do makepkg 7.1.0, não do recipe. Workaround
  documentado: MAKEPKG_LINT_PKGBUILD=0. build()/package() 100% válidos.

UX/sistema: vm-ui não oferece "Launch daemon" quando já conectado (evita
spawn duplicado com systemd); `vm actions` listava 14/16 ações — faltavam
setSoundboardMasterVolume e selectInput (corrigido).

Validado: desktop-file-validate OK; appstreamcli só avisa url placeholder
(repo ainda não público); 33/33 testes; release workspace OK.

## LANÇAMENTO v0.1.0 — preparado (2026-10-15)
- `.gitignore` criado (target/, artefatos de build); `git init` + commit
  inicial + **tag anotada v0.1.0** (f9541fb). Nada de target/ versionado
  (57 arquivos, 0 de build).
- CHANGELOG.md (Keep a Changelog) + RELEASE.md (checklist de publicação:
  remote, push, gh release, PR flathub, AUR, AppImage no release).
- URLs canônicas `github.com/jmiranda-sre/voicemod-linux` consistentes em
  metainfo/desktop/systemd/PKGBUILD/flatpak/README/CHANGELOG/RELEASE.
- Honestidade de escopo: metainfo e README NÃO prometem mais "global
  hotkeys" como feature — infra evdev existe (Keybind/modelo/watch_device)
  mas gerente nasce SEM binds e não há ação de API para criá-los
  (persistência é backlog). Troubleshooting row ajustada.
- README finalizado: exemplo de cliente WebSocket stdlib-only (testado
  contra daemon real: handshake 101, id ecoado, evento broadcast),
  seções "to install" (flatpak/appimage/aur) vs "to build", VM_INPUT_UID,
  config dir do sandbox flatpak.

## FASE 7 — Presets de fábrica + seção de dinâmica do microfone (2026-10-15)

Pedido: presets prontos "baixar e já usar" a partir de 2 configs EasyEffects
do usuário (fifine_masc_voice_noise_reduction.json do Downloads + gist
"Masc NPR Voice + Noise Reduction" — os dois são **idênticos**, mesma cadeia).

Diagnóstico inicial: o engine só tinha `null/gain/mute/mix/biquad/pitch-fft/
delay/formant/reverb/tube/oscillator` — a cadeia EasyEffects pedia rnnoise,
deepfilternet, gate, equalizer, compressor, deesser, limiter. Usuário escolheu
**port completo** (pergunta de escopo registrada na sessão).

Entregue:
- `vm-dsp/src/dynamics.rs`: **gate** (RMS simétrico + histerese), **compressor**
  (curve soft-knee em dB, makeup estático, ratio 1 = no-op exato), **limiter**
  (ataque instantâneo, release exponencial, teto nunca cruza o threshold),
  **de-esser** (split por lowpass RBJ, faixa alta = `x − low` EXATA por
  construção — versão inicial com highpass deu fase de 56° em 6.3kHz e a
  redução se desfazia; detector nessa faixa).
- `vm-dsp/src/denoise.rs`: supressor espectral (STFT 1024/hop 256 Hann+WOLA,
  ~21ms latência) — piso de ruído por bin com queda rápida/subida rápida só
  em frame QUIETO (< −40dBFS, gate de nível: fala sustentada nunca vira
  "ruído"), ganho Wiener com sobre-subtração + piso `reduction_db`, suavização
  temporal ataque/release + smooth espectral 3 taps (antimusical noise).
- `biquad` ganhou tipo runtime (lowpass/highpass/peaking|bell/lowshelf/
  highshelf/bandpass/notch + freq/q/gain_db, RBJ, recomputa em set_param ou
  troca de rate) — o "EQ work" que ficou pendente desde a Fase 3b.
- `vm-preset`: `KNOWN_NODE_TYPES` + `BUILTIN_PRESETS` (include_str! de
  `builtin/mic-npr-voice.json`); `vm-daemon`: `seed_builtin_presets` no boot
  (nunca sobrescreve — edição do usuário vence).
- Preset `mic-npr-voice` mapeando a cadeia 1:1: denoise(strength 2.0,
  reduction −35) → gate(−50/−12/5/250/hist 3) → HPF 80 ×2 (slope x2 do
  EasyEffects) → bells 220 −2 (Q0.7) / 350 −2 (Q1.2) / 3500 +2 (Q0.9) →
  shelf 10k +2 → comp(−18/3:1/15/200/knee6/+3) → deess(4k/−22/3:1) →
  limiter(−1.5/release 5).

Bugs reais pegos pelos testes (com lição):
- **`db_to_gain` errado**: `exp2(db/20)` = 2^(dB/20), faltava ×log2(10)
  (10^(dB/20)). Os testes de dynamics passaram primeiro porque usavam a MESMA
  fórmula como oráculo (circularidade); o bell/shelf (oráculo hardcoded
  1.995) denunciou. Corrigido produção + todos os oráculos → `10f32.powf`.
- Detector RMS assimétrico (ataque rápido/lenta) enviesava nível em seno
  ~+1.2dB → virou one-pole SIMÉTRICO (ballistics é papel do ganho, não do
  detector).
- Fase do split do de-esser (acima).

Validado: **55/55 testes workspace** (+22: unidades de todos os processadores,
validação dos builtins, seed-preserva-edição, E2E da cadeia completa com ruído
branco pelo GraphProcessor — saída finita e ruído estacionário atenuado).
Latência: denoise adiciona ~21ms ao caminho do preset de mic (E2E ~72ms com a
cadeia espectral; medir de verdade = item 1 do backlog).

Entrega fechada com a **release 0.2.0** (bump minor por semver — features
novas em 0.x): ver PUBLICADO 0.2.0 abaixo.

## BACKLOG PÓS-LANÇAMENTO (itens NÃO entregues no 0.1.0 — registry p/ issues)
Dívidas Fase 4 (medição/latência):
1. Re-medir latência absoluta com voz ESPECTRAL ativa (STFT 2048 no caminho
   — hoje só medido com tom seco, ~51ms por exclusão de aliasing).
2. STFT adaptativo 2048→1024/512 (qualidade vs latência; 43ms só em vozes
   espectrais hoje).
3. Forçar quantum ≤960 no null-sink se a latência absoluta exigir; dedup
   final dos links do monitor.
Features:
4. Hotkeys: binds configuráveis (ação de API tipo addHotkey/setKeybind) +
   persistência JSON em $VM_CONFIG_DIR + UI de configuração.
5. `VM_INPUT_UID` virar setting persistido da UI (hoje só env/API).
6. Vozes IA/ONNX (pós-MVP declarado desde o plano; WinML ficou fora).
7. Crates/presets extras clássicos: chorus, vocoder, delay estéreo
   (Delay hoje compartilha linha entre canais).
8. Daemon: detectar porta 8765 já em uso e logar claro (hoje a segunda
   instância morre silenciosamente no bind).
Publicação:
9. Criar repo github.com/jmiranda-sre/voicemod-linux, push + tag, gh release
   (RELEASE.md passo a passo) — destrava metainfo/url e tag do flatpak.
10. Submeter Flathub (manifest pronto) e AUR (PKGBUILD pronto; .SRCINFO
    via MAKEPKG_LINT_PKGBUILD=0 por bug do pacman 7.1.0).
11. CI: matrix de build + publish AppImage no release.
12. pacman ≥7.1: revalidar pkgver local quando a regressão for corrigida
    (remover nota do PKGBUILD se o upstream consertar).
13. denoise: refinamentos pós-0.1.1 — VAD/acima de −40dBFS mais fino,
    suavização espectral adaptativa, warmup < 1s (hoje piso semeia no
    primeiro frame); possivelmente RNNoise/DeepFilterNet via ONNX (item 6).
14. de-esser: modelar banda de detecção f1..f2 do EasyEffects (4k–8k com
    Q) — hoje detecta tudo acima do corte em 4k; limiter com lookahead
    opcional (EasyEffects usa 2ms).
Pós-v0.3.0 (brainstorm 2026-10-15, arquivados p/ v0.4.0+):
15. Troca automática de preset por app/cena (detecção do destino
    PipeWire/foco; fallback p/ padrão).
16. Denoisers neurais opcionais via ONNX (RNNoise/DeepFilterNet; ver
    item 6 acima — fundir ao implementar).
17. Assistente de calibragem de ganho (wizard fala-5s → gate/makeup/
    limiter + medidor de clipping).
18. Integração OBS via WebSocket (preset por cena, dock VU, soundboard
    em fontes).
19. Marketplace de presets (bundles assinados, instalar em 1 clique).
20. Modo filtro PipeWire nativo (alternativa ao null-sink; menos cópias,
    mais robustez a hotplug).

## PUBLICADO — v0.1.0 no ar (2026-10-15)
- Autenticação: `gh auth login` (HTTPS + credential helper do gh) como
  **jmiranda-sre**; sem chave SSH na máquina (rota SSH abandonada).
- Org `voicemodlinux` NÃO existe e usuário não pertence a nenhuma org →
  repo criado na conta pessoal: **github.com/jmiranda-sre/voicemod-linux**
  (público). ID canônico do Flatpak (`io.github.voicemodlinux.vm`) mantido;
  se no futuro criar a org, transfer mantém redirects e só as referências
  de URL precisam ser atualizadas.
- URLs trocadas em 7 arquivos (CHANGELOG/RELEASE/STATUS/PKGBUILD/flatpak
  manifest/metainfo/systemd unit) — metainfo passa a validar 100%
  (appstreamcli ✔ sem avisos; antes só avisava URL inexistente).
- Commit de release re-amendado (6c5955c) + tag v0.1.0 recriada;
  push main + tag OK (`git ls-remote --tags origin` confirma).
- Release criada: github.com/jmiranda-sre/voicemod-linux/releases/tag/v0.1.0
  (não-draft, não-prerelease) com VoicemodLinux-0.1.0-x86_64.AppImage
  (16.431.608 bytes) anexado + notas do CHANGELOG.
- Próximo (do usuário): PR no Flathub (manifest pronto, tag resolve),
  AUR (PKGBUILD pronto, .SRCINFO com MAKEPKG_LINT_PKGBUILD=0),
  Issue tracker para o backlog do STATUS.md.

## PUBLICADO — v0.2.0 no ar (2026-10-15)
- Bump em 4 arquivos (workspace Cargo.toml + Cargo.lock, metainfo com nova
  entrada `<release version="0.2.0">`, manifest Flatpak `tag: v0.2.0` +
  fix da URL do x-checker-data que apontava pra `voicemodlinux/app`
  inexistente). PKGBUILD não muda: pkgver deriva de `git describe`.
- `appstreamcli validate` ✔ 100% (URL da repo agora resolve); 55/55
  testes; clippy dos crates novos limpo (resta só débito pré-existente:
  spectral.rs/graph.rs/vm-audio/vm-soundboard).
- AppImage reconstruído pelo `packaging/appimage/build.sh`:
  VoicemodLinux-0.2.0-x86_64.AppImage (16.439.800 bytes) — **contém os
  presets de fábrica** (seed no boot), ao contrário do 0.1.0.
- Commits `08edfbe` (feat/dsp) + `8c09082` (release) + tag anotada
  `v0.2.0`; push main + tag OK; Release criada
  github.com/jmiranda-sre/voicemod-linux/releases/tag/v0.2.0
  (não-draft, não-prerelease) com AppImage anexado + notas do CHANGELOG.
- Smoke real pós-build (daemon release, backend stub, config dir novo):
  seed de `mic-npr-voice`, `vm check-preset` → 11 nós/10 arestas,
  `selectVoice` → `ok:true` + evento `voiceChanged`, `setVoiceParameter`
  em comp/denoise/limit → `ok:true`, nó inexistente → erro limpo.
- Aviso conhecido p/ clientes: mensagem que falha no parse responde
  `ok:false` SEM ecoar `id` (o id pertence ao payload inválido) — não é
  regressão, já era assim no 0.1.0.
- Próximo (do usuário): PR no Flathub (manifest pronto, tag v0.2.0
  resolve), AUR (PKGBUILD pronto), issues do backlog.

## v0.3.0 — ROADMAP APROVADO (planejamento, 2026-10-15 — SEM CÓDIGO AINDA)
Escopo: 7 itens do brainstorm (numeração original entre parênteses).
Foco: usabilidade + desempenho + integração. Nada implementado — frentes
abaixo são o plano proposto p/ aprovação.

### F-A — EM ANDAMENTO (telemetria + watchdog)
(Ordem aprovada: instrumentação primeiro.)
- `vm-dsp` `Graph`: `NodeTelemetry` por nó (avg EMA α=1/16 + max + calls,
  `Instant::now` vDSO, vec pré-dimensionado no `add` — zero alloc no RT),
  `telemetry()` p/ o control plane, `is_empty()` (fecha o lint do clippy).
- `vm-daemon`: `graph_xruns` (contenção de `try_lock`), ação `GetHealth`
  (voice/link/graphXruns/nodes[]/capture/playback/monitor/watchdog{}),
  `WatchdogState` + política pura `decide_watchdog` (sem topologia → nada;
  cooldown 10s; `NeedsRestart` → restart; storm ≥20 xruns/tick → restart)
  + `restart_pipeline` (stop→open→start, grafo intacto) no tick de 2s.
- `vm-control`: ação `GetHealth` no catálogo; `serve()` loga cada porta
  ocupada antes de cair p/ a próxima (fecha backlog 8).
- `vm-cli`: `vm actions` lista `getHealth` (17 ações).
- `vm-ui`: structs `HealthInfo/NodeHealth/StreamHealth`, `getHealth` no
  bootstrap + refresh, aba *Health* (tabela µs por nó, streams c/ xrun
  em vermelho, restarts + último motivo).
- Testes novos: telemetria (calls/picos), health shape, contenção conta
  xrun, matriz da política, tick quieto no stub + reopen direto.

### F-B — FECHADA (hotkeys, PTT/mute, persistência) — 2026-10-15
- `vm-control`: alvos momentâneos `PushToTalk`/`PushToMute` +
  `is_momentary`; `watch_device` entrega `(target, pressed)` (release só
  p/ chords momentâneos que deixam de casar); `StoredBind` + `export`/
  `import` (ids estáveis); `save/load_config` (versão 1, falha fechada);
  `validate_bind`; `sandbox_note` (Flatpak → mensagem própria).
- `vm-daemon`: flag `muted` (zera saída, DSP quente); `SetMuted` +
  evento `MutedChanged`; `Add/Remove/ListHotkeys` com persistência
  atômica (snapshot → aplica → salva → rollback); `config.json` c/
  quarentena `.bak`; `hotkey_action(target, pressed)`; boot carrega
  binds antes das threads evdev.
- Bug real achado no smoke: campo `id` do `removeHotkey` colidia com o
  `id` de correlação (serde prioriza o envelope → parse falhava sem eco
  → cliente estrito travava; a UI sempre envia id). Renomeado p/
  `hotkey_id` + teste anti-colisão genérico p/ as 21 ações.
- Verificado ao vivo (release, stub): add/list/remove/setMuted,
  `config.json` em disco, reboot → bind id 1 sobrevive, corrompido →
  `.bak` + boot segue sem binds.
- Testes: roundtrip add/list/remove c/ arquivo, rejeições,
  reload pós-"reboot" (ids 0,1 → próximo 2), quarentena, mute
  bit-exato + evento, matriz de bordas, formato JSON no fio.

### F-8 — FECHADA (Stream Deck / Companion) — 2026-10-15
- `GetVoices` (stems ordenados p/ dropdowns); `GetHealth` += `enabled` +
  `muted` (init de botões sem togglar); `StopAllSounds` emite
  `SoundStopped` por som tocando (evento existia desde a Fase 3b, nunca
  emitido); `vm actions` = 22.
- `docs/streamdeck-companion.md`: mapa ação→botão, tabela evento→estado,
  exemplos de payload, sketch de módulo Companion, sessão de exemplo;
  README linka o doc e as listas de ações/eventos foram atualizadas
  (estavam defasadas desde a F-A/F-B).
- Testes: E2E socket real c/ handler real (select→voiceChanged,
  mute→mutedChanged, toggle→evento, health, voices, ids estritos);
  anti-colisão 22/22 ações (23 após F-3: `captureNoiseProfile`).

### F-3 — FECHADA (perfil de ruído salvo) — 2026-10-15
- `JsonParamValue::FloatArray` (última variante untagged) +
  `ParamValue::FloatArray` + `AudioNode::get_param` (default: None).
  Scalars rejeitam arrays (`TypeMismatch`, nunca coerção silenciosa).
- `denoise`: param `profile` (513 bins; instala em todos os canais +
  `pending_profile` p/ canais criados depois — sem o stash o install
  do boot era no-op silencioso, pego pelos testes); `get_param`
  devolve o floor do canal 0; bins inválidos sanitizam p/ 0.
- `CaptureNoiseProfile{node}` ("learn room"): lê o floor vivo e grava
  no preset da voz atual como `profile` não-público/sem range
  (re-validado antes do disco; demais params intactos). Próximo load =
  warmup instantâneo; sem perfil = aprendizado dinâmico (teste
  explícito de retrocompat).
- UI: linha read-only "N bins stored"; `vm actions` = 23.
- Testes: schema array (roundtrip/validação), install+readback+
  validação, skip do first-frame seeding (primed passa 5×+ no início),
  aprendizado sem perfil, captura→arquivo→reload, rejeições.

### F-5 — FECHADA (baixa latência + ms por nó) — 2026-10-15
- `AudioNode::latency_samples` (default 0): denoise = frame ativo,
  pitch/formant = 2048, delay = linha; `Graph` soma o budget da cadeia.
- Denoise `frame` 512/1024 (default 1024): planos FFT + janela
  refeitos no control plane; troca reseta canais mas carrega o floor
  vivo (reamostrado) — sem re-learn; perfil reamostra nos dois
  sentidos (nunca bricka load); `get_param` devolve prefixo ativo.
- `GetHealth`: `latMs` por nó + `latencyMs` total; UI mostra coluna
  lat ms, total da cadeia e nota do tradeoff de graves (512 = bins de
  93.8 Hz); `mic-npr-voice` com `frame: 1024` explícito e público.
- Testes: switch 512↔1024 + replan + rejeição de 777, resample de
  perfil, supressão mantida em 512, soma de latência, health latMs via
  API, E2E cadeia 512 (finito + atenua, xruns de contenção = 0).
- Medição absoluta fim-a-fim em HW real segue no backlog item 1 (stub
  não gera xruns; prova aqui = budget algorítmico + CPU sã).

### F-6 — FECHADA (pack de vozes-personagem) — 2026-10-15
- 5 builtins (`robot`, `chipmunk`, `demon`, `radio`, `telephone`),
  receitas próprias só com nós existentes; `BUILTIN_PRESETS` = 6 e o
  seed do boot as instala sozinho (nunca sobrescreve).
- Teste de builtins endurecido: 6 arquivos, edges = nós−1, public_params
  pontuados; `vm check-preset` OK nos 5 via binário release.
- Golden por persona: chipmunk 740 Hz ±5%, demon 293.7 Hz ±7%, robot
  colore sem silenciar, radio/telefone passam 1 kHz melhor que 100 Hz
  (tube gera harmônicos do grave — teste mede a física, não o chute:
  direcional ×1.5 + rejeição absoluta <0.5; telephone vaza menos grave
  que o radio).

### F-13 — FECHADA (autostart + tray) — 2026-10-15/16
- `vm autostart`: entry XDG (`--minimized`) + unit user; falha parcial
  honesta; sandbox → guia de portal (Background permission).
- Tray SNI via `ksni` (puro-Rust, sem GTK): ícone em código, menu
  Show/mute/Quit, left-click alterna; close esconde (link WS intacto),
  Quit sai de verdade; `--minimized` nasce escondido; sem tray
  (sandbox/sem host) = janela normal.
- Decisão de peso: backend AppIndicator default puxava ICU+GTK
  (AppImage 45.6 MB) → `ksni` = 17.2 MB; sem XEmbed legado (hosts SNI:
  Plasma, GNOME + extensão).
- Snapshot UI += `muted` (evento + seed do health) p/ o checkmark.
- Unit validada (`systemd-analyze verify` só reclama do binário, que
  não está instalado aqui); desktop renderizado passa no
  `desktop-file-validate`; ciclo enable→status→disable provado ao vivo.
- Testes: ícone 32px RGBA, flag `--minimized`, entry/install/remove/
  lookup/status puros; tray runtime não executado sem display (nota
  honesta — D-Bus/SNI do usuário intocados).

### Ordem de execução sugerida (com motivo)
1. **(11) Dashboard de saúde RT + watchdog** — PRIMEIRO porque é a
   instrumentação que todos os outros usam p/ se validar (xruns, CPU/nó,
   clipping, estado do link). Sem isso, o item 5 não tem como provar
   ganho de latência. Dívidas que ele absorve: backlog 8 (porta em uso
   → log claro) e o espírito do backlog 1 (medição).
2. **(9) Hotkeys globais evdev + PTT/mute + persistência** — fecha a
   dívida desde a v0.1.0 (backlog 4 vira esta frente) e ESTENDE a
   superfície da Control API (ações add/remove/list binds). Vem antes
   do item 8 para o Stream Deck já nascer cobrindo os binds novos.
3. **(8) Stream Deck / Companion** — consome a API estabilizada no
   passo anterior: documenta o mapeamento ação→botão, garante feedback
   via eventos existentes (`voiceChanged` etc.) e GetBitmap honesto.
   Quase só cola + docs + testes de protocolo.
4. **(3) Perfil de ruído da sala salvo no preset** — DSP autocontido
   (snapshot do floor em `$VM_CONFIG_DIR`, reload no boot, versão do
   formato p/ invalidar perfis velhos). Validado pelo dashboard do
   passo 1 (warmup ~1s → ~0). Absorve a parte "warmup" do backlog 13.
5. **(5) Modo baixa latência + ms por nó na UI** — usa a medição do
   passo 1: chave STFT 512/1024 no denoise (+ talvez quantum), mostra
   custo por nó. Absorve backlog 1 (re-medir c/ espectral ativa) e 2
   (STFT adaptativo).
6. **(6) Pack de vozes-personagem de fábrica** — SÓ CONTEÚDO (nós já
   existem: pitch/formant/reverb/tube/delay): robô, demônio, rádio,
   chipmunk, etc. via `BUILTIN_PRESETS`. Paralelizável a qualquer
   momento; colocado aqui porque é entrega visível de fim de ciclo.
   Critério: cada preset valida em `vm check-preset` + teste golden
   (ex.: +12st → pico em 880Hz ±5%, como já fazemos).
7. **(13) Autostart + tray** — POR ÚLTIMO: é integração/empacotamento
   (systemd user unit, `.desktop`, portal Flatpak, tray no egui) e deve
   coroar o daemon com comportamento final — autostartar algo ainda em
   mudança gera bug fantasma. Inclui "não oferecer Launch c/ systemd
   ativo" (lição da Fase 6).

### Frentes de trabalho (o quê / onde toca / aceite)
- **F-A observabilidade (11)**: telemetria por nó (µs + xruns) no
  `GraphProcessor`, evento/status na API, aba "Health" na UI, watchdog
  c/ reconexão (modelo: `TryRestartOnInternalError` do original, Fase 2).
  Aceite: forçar xrun artificial → UI mostra + reconecta sem restart.
- **F-B controle físico (9+8)**: `Keybind` persistido em JSON + ações
  `addHotkey/removeHotkey/listHotkeys` + perfis por preset
  (PTT/PTM/mute); doc de mapeamento Stream Deck/Companion + fixture de
  teste de protocolo. Aceite: bind sobrevive a restart; botão físico
  (simulado via evento) troca voz. Sandbox: degrada sem /dev/input
  (já faz — Fase 6).
- **F-C DSP de mic (3+5)**: snapshot de floor + chave de frame + tabela
  de latência por nó. Aceite: 2º boot sem warmup audível; UI mostra ms
  por nó somando ≈ medido fim-a-fim (±20%).
- **F-D conteúdo (6)**: N presets curados + docs. Aceite: todos passam
  em `check-preset` + golden de pitch.
- **F-E sistema (13)**: unit user + tray + autostart opt-in + docs de
  sandbox (Flatpak sem tray/evdev = degradação documentada, não bug).
  Aceite: login → daemon ativo → vm-mic selecionável sem abrir UI.

### Riscos conhecidos
- evdev precisa grupo `input`/udev (já documentado Fase 3); Flatpak não
  tem /dev/input nem tray pleno — escopo nativo/AUR+AppImage, resto
  degrada com mensagem honesta.
- STFT 512 piora resolução de graves do denoise — o modo baixa latência
  nasce como OPT-IN com o trade-off escrito na UI, não default.
- Hotkeys globais fora de jogo Wayland: evdev lê hardware, não
  compositor — funciona, mas testar em CachyOS/KWin antes de prometer.
