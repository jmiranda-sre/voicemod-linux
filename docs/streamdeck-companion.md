# Stream Deck / Bitfocus Companion integration

`vm-daemon` exposes its full control surface over a local WebSocket
(`ws://127.0.0.1:8765`, falls through `8766`–`8769` when busy — the
effective port is logged). Any hardware button that can speak WebSocket
+ JSON (Stream Deck plugin with a WS client, Bitfocus Companion generic
modules) drives the daemon with no extra bridge process.

Wire rules (shared by every client):

- Request: `{"action": "...", ...fields, "id": 1}` — `id` is optional
  but recommended; the reply echoes it flat (`{"ok":…,"id":1}`).
- Reply: `{"ok": true, "data": {...}}` or `{"ok": false, "error": "…"}`.
- Broadcasts: `{"event": "...", ...}` fan out to **every** connected
  client — this is the button-state feedback channel.
- Auth is opt-in (`VM_CONTROL_TOKEN` on the daemon, `"token"` per
  message). For a second device / Stream Deck on another host, set the
  token and give the plugin the same value.
- Field names never collide with the envelope (`id`/`token`) — there
  is a compile-time test over all 22 actions enforcing this
  (a `removeHotkey` bug of exactly this class was caught in F-B).

## Bootstrap (what a module does on connect)

1. Connect to the first reachable port in `8765…8769`.
2. `getVoices` → populate voice dropdowns.
3. `getHealth` → init all button states at once (`voice`, `enabled`,
   `muted`, per-node cost, stream xruns).
4. `getAllSoundboard` → populate sound buttons.
5. `listHotkeys` → show which physical keys are already bound.

## Action map (button → payload)

| Button | Send | Feedback event |
|---|---|---|
| Voice: *Mic NPR* | `{"action":"selectVoice","voice":"mic-npr-voice"}` | `voiceChanged` → highlight active voice |
| Changer on/off | `{"action":"toggleVoiceChanger","enabled":false}` | `voiceChangerToggled` |
| Mic mute toggle | `{"action":"setMuted","muted":true}` | `mutedChanged` |
| Play sting | `{"action":"playSound","sound":"sting"}` | `soundStarted` |
| Stop everything | `{"action":"stopAllSounds"}` | one `soundStopped` per playing sound |
| EQ air +2 dB | `{"action":"setVoiceParameter","param":"air.gain_db","value":2.0}` | `voiceParameterChanged` (echoes `param`+`value`) |
| Reload voice | `{"action":"resetVoiceParameters"}` | `voiceChanged` |
| Start stream | `{"action":"startAudioStreaming"}` | `streamStatus{active:true}` |
| Bind PTT key | `{"action":"addHotkey","codes":[97],"target":"PushToTalk","enabled":true}` | reply `data.id` (stable bind id) |
| Unbind | `{"action":"removeHotkey","hotkey_id":0}` | `ok:true` |

Notes:

- `param` is always `"node.param"` (`comp.makeup_db`, `denoise.strength`,
  `limit.threshold_db` …); values are number/bool/string per the preset.
- `getAudioStreamStatus` stays the lightweight poll (active/voice/xruns);
  `getHealth` is the rich snapshot (nodes, streams, watchdog).
- Sounds are fire-and-forget: `soundStarted` fires on trigger,
  `soundStopped` fires per sound on `stopAllSounds`. Natural end-of-play
  emits nothing — model sound buttons as momentary, not toggles.
- `getBitmap` honestly errors (no icon assets in the clean-room build):
  ship PNGs with your module instead.
- evdev codes are Linux `KEY_*` numbers (`KEY_RIGHTCTRL = 97`,
  `KEY_F9 = 67`); binds persist daemon-side in `config.json`.

## Companion module sketch

- **Config**: host, port list (`8765-8769`), optional token. Probe ports
  in order; remember the effective one; reconnect with backoff.
- **Actions**: one per table row above (voice/sound dropdowns fed by
  `getVoices`/`getAllSoundboard`).
- **Feedbacks**: `voice` == selected (from `voiceChanged` + `getHealth`
  at connect), `enabled` (from `voiceChangerToggled`), `muted` (from
  `mutedChanged`), `playing` (set on `soundStarted`, clear on matching
  `soundStopped`).
- **Presets (button templates)**: "Voice: X" (press → selectVoice,
  feedback → highlight), "Mute toggle" (press → flip last `muted`,
  feedback → red), "PTT" (down → `toggleVoiceChanger{true}`, up →
  `{false}`), "Sting" (press → playSound).

## Worked session

```
→ {"action":"getVoices","id":1}
← {"ok":true,"data":{"voices":["example-drive","mic-npr-voice"]},"id":1}
→ {"action":"selectVoice","voice":"mic-npr-voice","id":2}
← {"ok":true,"id":2}
← {"event":"voiceChanged","voice":"mic-npr-voice"}
→ {"action":"setVoiceParameter","param":"comp.makeup_db","value":6.0,"id":3}
← {"ok":true,"id":3}
← {"event":"voiceParameterChanged","param":"comp.makeup_db","value":6.0}
→ {"action":"setMuted","muted":true,"id":4}
← {"ok":true,"id":4}
← {"event":"mutedChanged","muted":true}
```

The daemon's own test-suite replays this exact loop over a real socket
(`integration_tests::websocket_control_and_feedback_loop`) — if the
protocol drifts, CI-equivalent `cargo test` goes red.
