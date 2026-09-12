// SPDX-License-Identifier: GPL-3.0-or-later
// Renderer-side consumer of the KDE Wallpaper Engine normalized input
// protocol v1 (kde/docs/INPUT_PROTOCOL_V1.md). The supervisor writes one
// compact JSON line per event onto the worker's stdin; pointer and
// render_pause events are acknowledged on stdout as
// `{"version":1,"type":"input_ack","sequence":N}`.
module;

#include <fcntl.h>
#include <unistd.h>

#include <chrono>
#include <cstdint>
#include <cstdio>
#include <functional>
#include <optional>
#include <string>
#include <vector>

export module kwe.input_wire;

import wescene.json;
import rstd;
import rstd.cppstd;

export namespace kwe
{

inline constexpr size_t kMaxInputMessageBytes = 4096;

enum class PointerPhase
{
    Enter,
    Move,
    Leave,
    Down,
    Up,
};

enum class PointerButton
{
    Primary,
    Secondary,
    Middle,
};

struct PointerEvent {
    uint64_t                     sequence { 0 };
    PointerPhase                 phase { PointerPhase::Move };
    uint16_t                     x { 0 };
    uint16_t                     y { 0 };
    std::optional<PointerButton> button;
};

struct MediaStateEvent {
    uint64_t    sequence { 0 };
    std::string playback; // "playing" | "paused" | "stopped"
    std::string title;
    std::string artist;
    std::string album;
};

// F3: the daemon's render pause/resume verdict. A paused worker must stop
// simulating/painting but keep re-publishing its last frame at a bounded
// keepalive rate (kRenderPauseKeepalive) so the supervisor's watchdog stays
// meaningful. `sequence` carries the display generation, like media_state.
struct RenderPauseEvent {
    uint64_t sequence { 0 };
    bool     paused { false };
};

inline constexpr auto kRenderPauseKeepalive = std::chrono::milliseconds(500);

struct InputCallbacks {
    std::function<void(const PointerEvent&)>     on_pointer;
    std::function<void(const MediaStateEvent&)>  on_media_state;
    std::function<void(const RenderPauseEvent&)> on_render_pause;
    // Bands per channel (16/32/64 f32 values in 0..=1). Sequence carries the
    // display generation, never validated for monotonicity.
    std::function<void(const std::vector<float>& left, const std::vector<float>& right)>
        on_audio_bands;
};

// Nonblocking NDJSON reader over the daemon-owned stdin pipe with the
// matching stdout acknowledgement writer. Mirrors the bounded consumption
// pattern of the Rust workers: at most 4 message-sized chunks per poll, an
// over-long unterminated buffer is discarded, unknown types are ignored at
// the framing boundary.
class InputChannel {
public:
    InputChannel() {
        setNonblocking(STDIN_FILENO);
        setNonblocking(STDOUT_FILENO);
        m_buffer.reserve(kMaxInputMessageBytes);
    }

    void Poll(const InputCallbacks& callbacks) {
        size_t total = 0;
        while (total < kMaxInputMessageBytes * 4) {
            char          chunk[kMaxInputMessageBytes];
            const ssize_t read = ::read(STDIN_FILENO, chunk, sizeof(chunk));
            if (read <= 0) break;
            total += size_t(read);
            m_buffer.append(chunk, size_t(read));
            size_t newline = 0;
            while ((newline = m_buffer.find('\n')) != std::string::npos) {
                std::string line = m_buffer.substr(0, newline);
                m_buffer.erase(0, newline + 1);
                if (! line.empty() && line.size() <= kMaxInputMessageBytes) {
                    dispatchLine(line, callbacks);
                }
            }
            if (m_buffer.size() > kMaxInputMessageBytes) m_buffer.clear();
        }
    }

private:
    static void setNonblocking(int descriptor) {
        const int flags = ::fcntl(descriptor, F_GETFL);
        if (flags >= 0) (void)::fcntl(descriptor, F_SETFL, flags | O_NONBLOCK);
    }

    static void writeAck(uint64_t sequence) {
        char line[96];
        const int written = std::snprintf(
            line, sizeof(line), "{\"version\":1,\"type\":\"input_ack\",\"sequence\":%llu}\n",
            (unsigned long long)sequence);
        if (written > 0) {
            // Best-effort on the nonblocking daemon ack pipe; a full pipe
            // drops the ack, exactly like the Rust workers.
            (void)::write(STDOUT_FILENO, line, size_t(written));
        }
    }

    // Sequence numbers arrive as JSON numbers; doubles are exact through
    // 2^53, far beyond any daemon-issued sequence.
    static std::optional<uint64_t> readSequence(const owe::Json& parsed) {
        double sequence = -1.0;
        if (! owe::GetJsonValue(parsed, "sequence", sequence, /*warn=*/false)) {
            return std::nullopt;
        }
        if (sequence < 0.0) return std::nullopt;
        return uint64_t(sequence);
    }

    static void dispatchLine(const std::string& line, const InputCallbacks& callbacks) {
        auto parsed_result = owe::ParseJson(line);
        if (parsed_result.is_err()) return;
        auto parsed = rstd::move(parsed_result).unwrap();
        if (! parsed.is_object()) return;

        rstd::uint32_t version = 0;
        if (! owe::GetJsonValue(parsed, "version", version, /*warn=*/false) || version != 1) {
            return;
        }
        std::string type;
        if (! owe::GetJsonValue(parsed, "type", type, /*warn=*/false)) return;

        if (type == "pointer_position") {
            dispatchPointer(parsed, callbacks);
        } else if (type == "media_state") {
            dispatchMediaState(parsed, callbacks);
        } else if (type == "audio_bands") {
            dispatchAudioBands(parsed, callbacks);
        } else if (type == "render_pause") {
            dispatchRenderPause(parsed, callbacks);
        }
        // Unknown types are ignored at the framing boundary per the protocol.
    }

    static void dispatchPointer(const owe::Json& parsed, const InputCallbacks& callbacks) {
        auto sequence = readSequence(parsed);
        if (! sequence) return;
        std::string    phase;
        rstd::uint32_t x = 0;
        rstd::uint32_t y = 0;
        if (! owe::GetJsonValue(parsed, "phase", phase, /*warn=*/false) ||
            ! owe::GetJsonValue(parsed, "x", x, /*warn=*/false) ||
            ! owe::GetJsonValue(parsed, "y", y, /*warn=*/false) || x > 0xffff || y > 0xffff) {
            return;
        }

        PointerEvent event;
        event.sequence = *sequence;
        event.x        = uint16_t(x);
        event.y        = uint16_t(y);
        if (phase == "enter") {
            event.phase = PointerPhase::Enter;
        } else if (phase == "move") {
            event.phase = PointerPhase::Move;
        } else if (phase == "leave") {
            event.phase = PointerPhase::Leave;
        } else if (phase == "down") {
            event.phase = PointerPhase::Down;
        } else if (phase == "up") {
            event.phase = PointerPhase::Up;
        } else {
            return;
        }
        std::string button;
        if (owe::GetJsonValue(parsed, "button", button, /*warn=*/false)) {
            if (button == "primary") {
                event.button = PointerButton::Primary;
            } else if (button == "secondary") {
                event.button = PointerButton::Secondary;
            } else if (button == "middle") {
                event.button = PointerButton::Middle;
            } else {
                return;
            }
        }
        // Down/up require a button; other phases must not carry one.
        const bool is_button_phase =
            event.phase == PointerPhase::Down || event.phase == PointerPhase::Up;
        if (is_button_phase != event.button.has_value()) return;

        if (callbacks.on_pointer) callbacks.on_pointer(event);
        writeAck(event.sequence);
    }

    static void dispatchMediaState(const owe::Json& parsed, const InputCallbacks& callbacks) {
        auto sequence = readSequence(parsed);
        if (! sequence) return;
        MediaStateEvent event;
        event.sequence = *sequence;
        if (! owe::GetJsonValue(parsed, "playback", event.playback, /*warn=*/false)) return;
        if (event.playback != "playing" && event.playback != "paused" &&
            event.playback != "stopped") {
            return;
        }
        (void)owe::GetJsonValue(parsed, "title", event.title, /*warn=*/false);
        (void)owe::GetJsonValue(parsed, "artist", event.artist, /*warn=*/false);
        (void)owe::GetJsonValue(parsed, "album", event.album, /*warn=*/false);
        if (callbacks.on_media_state) callbacks.on_media_state(event);
    }

    static void dispatchRenderPause(const owe::Json& parsed, const InputCallbacks& callbacks) {
        auto sequence = readSequence(parsed);
        if (! sequence || *sequence == 0) return;
        RenderPauseEvent event;
        event.sequence = *sequence;
        if (! owe::GetJsonValue(parsed, "paused", event.paused, /*warn=*/false)) return;
        if (callbacks.on_render_pause) callbacks.on_render_pause(event);
        writeAck(event.sequence);
    }

    static void dispatchAudioBands(const owe::Json& parsed, const InputCallbacks& callbacks) {
        if (! callbacks.on_audio_bands) return;
        std::vector<float> left;
        std::vector<float> right;
        if (! owe::GetJsonValue(parsed, "left", left, /*warn=*/false) ||
            ! owe::GetJsonValue(parsed, "right", right, /*warn=*/false)) {
            return;
        }
        const size_t count = left.size();
        if (count != right.size() || (count != 16 && count != 32 && count != 64)) return;
        for (const float value : left) {
            if (! (value >= 0.0f && value <= 1.0f)) return;
        }
        for (const float value : right) {
            if (! (value >= 0.0f && value <= 1.0f)) return;
        }
        callbacks.on_audio_bands(left, right);
    }

    std::string m_buffer;
};

} // namespace kwe
