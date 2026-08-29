// SPDX-License-Identifier: GPL-3.0-or-later
// Shared scaffolding for the OWE-backed KDE Wallpaper Engine renderer
// workers: the uniform supervisor argv contract (kde/docs/SUPERVISOR_API_V1.md),
// the development-only fault injection flags, SIGTERM handling, and frame
// pacing. Exit codes follow the worker contract: 0 graceful, 70 exit-after
// fault, 71/72 resource faults, 73 backend rejection, 74 no drawable content.
module;

#include <signal.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
#include <optional>
#include <string>
#include <string_view>
#include <thread>

export module kwe.worker_common;

import kwe.frame_protocol;

export namespace kwe
{

inline constexpr int kExitGraceful         = 0;
inline constexpr int kExitUsage            = 2;
inline constexpr int kExitFault            = 70;
inline constexpr int kExitAllocationDenied = 71;
inline constexpr int kExitAllocationOk     = 72;
inline constexpr int kExitBackendReject    = 73;
inline constexpr int kExitNoDrawable       = 74;

struct WorkerArgs {
    std::string output;
    uint32_t    width { 960 };
    uint32_t    height { 540 };
    uint32_t    fps { 30 };
    std::string scaling { "aspect" }; // aspect | fill | stretch
    std::string content;
    // Scene kind only.
    std::string assets_dir;
    std::string shader_helper; // accepted for argv compatibility; unused
    // Web kind only.
    uint64_t web_heartbeat_ms { 5000 };
    uint32_t web_heartbeat_max_failures { 3 };
    bool     allow_network { false };
    bool     probe { false };
    // Development-only fault flags (daemon gates them by --allow-test-faults).
    bool                    startup_hang { false };
    std::optional<uint64_t> hang_after;
    std::optional<uint64_t> corrupt_after;
    std::optional<uint64_t> exit_after;
    bool                    ignore_term { false };
    std::optional<uint64_t> memory_pressure_after;
    std::optional<uint64_t> memory_pressure_mib;
    std::optional<uint32_t> stderr_lines;
};

namespace detail
{

inline bool parseU64(const char* text, uint64_t& value) {
    if (! text || ! *text) return false;
    char*              end    = nullptr;
    errno              = 0;
    unsigned long long parsed = std::strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') return false;
    value = parsed;
    return true;
}

} // namespace detail

// Long-flag-only parser for the exact supervisor argv contract. `web`
// selects which kind-specific flags are accepted. Returns std::nullopt and
// writes a diagnostic on any unknown flag or malformed value.
inline std::optional<WorkerArgs> ParseWorkerArgs(int argc, char** argv, bool web,
                                                 std::string& error) {
    WorkerArgs args;
    auto       need_value = [&](int& index) -> const char* {
        if (index + 1 >= argc) return nullptr;
        return argv[++index];
    };
    for (int index = 1; index < argc; ++index) {
        const std::string_view flag = argv[index];
        auto take_u64 = [&](std::optional<uint64_t>& target) -> bool {
            uint64_t    value = 0;
            const char* text  = need_value(index);
            if (! detail::parseU64(text, value)) {
                error = std::string(flag) + " requires an unsigned integer";
                return false;
            }
            target = value;
            return true;
        };
        auto take_bounded = [&](uint64_t minimum, uint64_t maximum, uint64_t& target) -> bool {
            uint64_t    value = 0;
            const char* text  = need_value(index);
            if (! detail::parseU64(text, value) || value < minimum || value > maximum) {
                error = std::string(flag) + " must be in " + std::to_string(minimum) + ".." +
                        std::to_string(maximum);
                return false;
            }
            target = value;
            return true;
        };
        auto take_string = [&](std::string& target) -> bool {
            const char* text = need_value(index);
            if (! text) {
                error = std::string(flag) + " requires a value";
                return false;
            }
            target = text;
            return true;
        };

        if (flag == "--output") {
            if (! take_string(args.output)) return std::nullopt;
        } else if (flag == "--width") {
            uint64_t value = 0;
            if (! take_bounded(1, kMaxDimension, value)) return std::nullopt;
            args.width = uint32_t(value);
        } else if (flag == "--height") {
            uint64_t value = 0;
            if (! take_bounded(1, kMaxDimension, value)) return std::nullopt;
            args.height = uint32_t(value);
        } else if (flag == "--fps") {
            uint64_t value = 0;
            if (! take_bounded(1, 240, value)) return std::nullopt;
            args.fps = uint32_t(value);
        } else if (flag == "--scaling") {
            if (! take_string(args.scaling)) return std::nullopt;
            if (args.scaling != "aspect" && args.scaling != "fill" &&
                args.scaling != "stretch") {
                error = "--scaling must be aspect, fill, or stretch";
                return std::nullopt;
            }
        } else if (flag == "--content") {
            if (! take_string(args.content)) return std::nullopt;
        } else if (! web && flag == "--assets-dir") {
            if (! take_string(args.assets_dir)) return std::nullopt;
        } else if (! web && flag == "--shader-helper") {
            if (! take_string(args.shader_helper)) return std::nullopt;
        } else if (web && flag == "--web-heartbeat-ms") {
            uint64_t value = 0;
            if (! take_bounded(250, 60000, value)) return std::nullopt;
            args.web_heartbeat_ms = value;
        } else if (web && flag == "--web-heartbeat-max-failures") {
            uint64_t value = 0;
            if (! take_bounded(1, 10, value)) return std::nullopt;
            args.web_heartbeat_max_failures = uint32_t(value);
        } else if (web && flag == "--allow-network") {
            args.allow_network = true;
        } else if (web && flag == "--probe") {
            args.probe = true;
        } else if (flag == "--startup-hang") {
            args.startup_hang = true;
        } else if (flag == "--hang-after") {
            if (! take_u64(args.hang_after)) return std::nullopt;
        } else if (flag == "--corrupt-after") {
            if (! take_u64(args.corrupt_after)) return std::nullopt;
        } else if (flag == "--exit-after") {
            if (! take_u64(args.exit_after)) return std::nullopt;
        } else if (flag == "--ignore-term") {
            args.ignore_term = true;
        } else if (flag == "--memory-pressure-after") {
            if (! take_u64(args.memory_pressure_after)) return std::nullopt;
        } else if (flag == "--memory-pressure-mib") {
            uint64_t value = 0;
            if (! take_bounded(1, 4096, value)) return std::nullopt;
            args.memory_pressure_mib = value;
        } else if (flag == "--stderr-lines") {
            uint64_t value = 0;
            if (! take_bounded(1, 4096, value)) return std::nullopt;
            args.stderr_lines = uint32_t(value);
        } else {
            error = "unknown flag: " + std::string(flag);
            return std::nullopt;
        }
    }
    if (args.memory_pressure_after.has_value() != args.memory_pressure_mib.has_value()) {
        error = "--memory-pressure-after and --memory-pressure-mib must be supplied together";
        return std::nullopt;
    }
    if (! args.probe) {
        if (args.output.empty()) {
            error = "--output is required";
            return std::nullopt;
        }
        if (args.content.empty()) {
            error = "--content is required";
            return std::nullopt;
        }
    }
    return args;
}

inline std::atomic<bool>& TerminationFlag() {
    static std::atomic<bool> flag { false };
    return flag;
}

// SIGTERM → cooperative shutdown flag; --ignore-term installs SIG_IGN to
// exercise the supervisor's bounded SIGKILL fallback. Also arms the Linux
// parent-death signal so an orphaned worker never outlives its daemon.
inline void InstallWorkerSignals(bool ignore_term) {
    ::prctl(PR_SET_PDEATHSIG, SIGTERM);
    if (ignore_term) {
        ::signal(SIGTERM, SIG_IGN);
        return;
    }
    struct sigaction action {};
    action.sa_handler = [](int) {
        TerminationFlag().store(true, std::memory_order_release);
    };
    ::sigemptyset(&action.sa_mask);
    ::sigaction(SIGTERM, &action, nullptr);
    ::sigaction(SIGINT, &action, nullptr);
}

[[noreturn]] inline void ParkForever() {
    for (;;) {
        std::this_thread::sleep_for(std::chrono::seconds(60));
    }
}

// Applies the per-frame development fault flags exactly like
// kwe-test-renderer: exit 70, corrupt-then-park, park, and the bounded
// memory-pressure allocation (71 denied / 72 unexpectedly granted).
inline void ApplyPublishFaults(const WorkerArgs& args, uint64_t published,
                               SharedFrameWriter& writer) {
    if (args.exit_after && published >= *args.exit_after) {
        std::fprintf(stderr, "event=renderer.fault kind=exit frames=%llu\n",
                     (unsigned long long)published);
        std::exit(kExitFault);
    }
    if (args.corrupt_after && published >= *args.corrupt_after) {
        std::fprintf(stderr, "event=renderer.fault kind=corrupt frames=%llu\n",
                     (unsigned long long)published);
        writer.CorruptMagicForTest();
        ParkForever();
    }
    if (args.hang_after && published >= *args.hang_after) {
        std::fprintf(stderr, "event=renderer.fault kind=hang frames=%llu\n",
                     (unsigned long long)published);
        ParkForever();
    }
    if (args.memory_pressure_after && published >= *args.memory_pressure_after) {
        const uint64_t mib   = args.memory_pressure_mib.value_or(0);
        const size_t   bytes = size_t(mib) * 1024 * 1024;
        uint8_t*       probe = new (std::nothrow) uint8_t[bytes];
        if (probe == nullptr) {
            std::fprintf(
                stderr,
                "event=renderer.fault kind=memory_pressure outcome=allocation_denied mib=%llu\n",
                (unsigned long long)mib);
            std::exit(kExitAllocationDenied);
        }
        delete[] probe;
        std::fprintf(
            stderr,
            "event=renderer.fault kind=memory_pressure outcome=unexpected_success mib=%llu\n",
            (unsigned long long)mib);
        std::exit(kExitAllocationOk);
    }
}

inline void EmitStderrLines(const WorkerArgs& args) {
    if (! args.stderr_lines) return;
    for (uint32_t index = 0; index < *args.stderr_lines; ++index) {
        std::fprintf(stderr, "event=renderer.stderr_line index=%u\n", index);
    }
}

// Fixed-rate pacing helper: sleeps to the next publish deadline, resetting
// after a stall so a slow frame never causes a burst of catch-up publishes.
class FramePacer {
public:
    explicit FramePacer(uint32_t fps)
        : m_interval(std::chrono::duration_cast<std::chrono::steady_clock::duration>(
              std::chrono::duration<double>(1.0 / double(fps)))),
          m_deadline(std::chrono::steady_clock::now()) {}

    void WaitNext() {
        m_deadline += m_interval;
        const auto now = std::chrono::steady_clock::now();
        if (m_deadline > now) {
            std::this_thread::sleep_for(m_deadline - now);
        } else {
            m_deadline = now;
        }
    }

private:
    std::chrono::steady_clock::duration   m_interval;
    std::chrono::steady_clock::time_point m_deadline;
};

} // namespace kwe
