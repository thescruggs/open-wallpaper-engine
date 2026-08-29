// SPDX-License-Identifier: GPL-3.0-or-later
// kwe-web-renderer (open-wallpaper-engine backend): the KDE Wallpaper
// Engine web worker, rendered by the CEF-based weweb::BrowserHost instead
// of the retired bwrap + headless-Chromium CDP worker. The daemon spawns:
//
//   kwe-web-renderer --output <frame> --width W --height H --fps N \
//       --scaling aspect|fill|stretch --content <dir with index.html> \
//       --web-heartbeat-ms MS --web-heartbeat-max-failures N \
//       [--allow-network] [fault flags]
//   kwe-web-renderer --probe
//
// Network isolation: without --allow-network the worker moves itself (and
// therefore every CEF subprocess it re-execs) into a fresh user+network
// namespace before CEF initialises — the same no-network guarantee the old
// worker got from `bwrap --unshare-net`, without the bwrap dependency. A
// kernel that refuses unprivileged user namespaces fails the launch closed.
//
// Liveness: the worker kicks an explicit Invalidate() every publish tick,
// so a healthy compositor paints continuously even for static pages; a
// paint gap longer than heartbeat-ms x max-failures is treated as a wedged
// browser and exits 73, mirroring the old worker's CDP heartbeat.
//
// Exit codes: 0 graceful SIGTERM, 70 exit-after fault, 71/72 memory
// pressure fault, 73 backend rejection (browser bootstrap failure, wedged
// page, netns setup failure), 74 no drawable content (no entry html).

#include <sched.h>
#include <unistd.h>

#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <optional>
#include <string>
#include <vector>

import rstd;
import rstd.cppstd;
import rstd.log;
import weweb;
import kwe.frame_protocol;
import kwe.input_wire;
import kwe.worker_common;

namespace
{

namespace fs = std::filesystem;

fs::path executable_dir() {
    char    buffer[4096];
    ssize_t written = ::readlink("/proc/self/exe", buffer, sizeof(buffer) - 1);
    if (written <= 0) return fs::current_path();
    buffer[written] = '\0';
    return fs::path(buffer).parent_path();
}

// Installed layout: CEF resources are staged beside the real binary
// (/usr/lib/kde-wallpaper-engine/weweb). Dev layout: the binary sits alone
// in the lito build tree, so fall back to the loaded libcef.so's bundle.
// Returns true when the resources live away from the executable (the dev
// layout) — the caller must then disable the zygote so CEF children can
// still find ICU.
bool resolve_cef_dirs(fs::path& resources, fs::path& locales) {
    const fs::path  exe_dir = executable_dir();
    std::error_code ec;
    if (fs::is_regular_file(exe_dir / "icudtl.dat", ec)) {
        resources = exe_dir;
        locales   = exe_dir / "locales";
        return false;
    }
    if (weweb::BrowserHost::LocateCefResources(resources, locales)) return true;
    resources = exe_dir;
    locales   = exe_dir / "locales";
    return false;
}

bool write_proc_file(const char* path, const std::string& contents) {
    int fd = ::open(path, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return false;
    const ssize_t written = ::write(fd, contents.data(), contents.size());
    ::close(fd);
    return written == ssize_t(contents.size());
}

// Fresh user+network namespace: no interface but loopback-down, no route
// out, inherited by every CEF subprocess. The uid/gid maps keep the
// worker's identity so file access is unchanged.
bool isolate_network(std::string& error) {
    const uid_t uid = ::getuid();
    const gid_t gid = ::getgid();
    if (::unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) {
        error = std::string("unshare(CLONE_NEWUSER|CLONE_NEWNET): ") + std::strerror(errno);
        return false;
    }
    if (! write_proc_file("/proc/self/setgroups", "deny")) {
        error = "write /proc/self/setgroups failed";
        return false;
    }
    char map[64];
    std::snprintf(map, sizeof(map), "%u %u 1", (unsigned)gid, (unsigned)gid);
    if (! write_proc_file("/proc/self/gid_map", map)) {
        error = "write /proc/self/gid_map failed";
        return false;
    }
    std::snprintf(map, sizeof(map), "%u %u 1", (unsigned)uid, (unsigned)uid);
    if (! write_proc_file("/proc/self/uid_map", map)) {
        error = "write /proc/self/uid_map failed";
        return false;
    }
    return true;
}

std::string cache_dir() {
    const char* home = std::getenv("HOME");
    std::string base = home && *home ? std::string(home) : std::string("/tmp");
    return base + "/.cache/kwe-owe-web";
}

// Resolve the wallpaper's entry page: the Wallpaper Engine project.json
// when present, otherwise a plain index.html (the daemon's preflight
// guarantees one exists).
std::optional<weweb::WebManifest> resolve_manifest(const fs::path& content) {
    if (auto manifest = weweb::LoadWebManifest(content); manifest.is_some()) {
        return rstd::move(manifest).unwrap();
    }
    if (fs::is_regular_file(content / "index.html")) {
        weweb::WebManifest manifest;
        manifest.title      = content.filename().string();
        manifest.entry_html = "index.html";
        return manifest;
    }
    return std::nullopt;
}

struct PaintState {
    std::vector<uint8_t> staging;
    kwe::FrameSpec       spec;
    bool                 have_frame { false };
    std::chrono::steady_clock::time_point last_paint {};

    void CopyFrame(const weweb::CpuPaintFrame& frame) {
        const uint32_t rows =
            uint32_t(frame.height) < spec.height ? uint32_t(frame.height) : spec.height;
        const uint32_t columns =
            uint32_t(frame.width) < spec.width ? uint32_t(frame.width) : spec.width;
        const uint8_t* source = static_cast<const uint8_t*>(frame.buffer);
        const bool     bgra   = frame.format == weweb::DmaBufFormat::BGRA8_UNORM;
        for (uint32_t row = 0; row < rows; ++row) {
            const uint8_t* in  = source + size_t(row) * frame.row_stride;
            uint8_t*       out = staging.data() + size_t(row) * spec.stride;
            if (bgra) {
                std::memcpy(out, in, size_t(columns) * 4);
            } else {
                for (uint32_t column = 0; column < columns; ++column) {
                    out[column * 4 + 0] = in[column * 4 + 2];
                    out[column * 4 + 1] = in[column * 4 + 1];
                    out[column * 4 + 2] = in[column * 4 + 0];
                    out[column * 4 + 3] = in[column * 4 + 3];
                }
            }
        }
        have_frame = true;
        last_paint = std::chrono::steady_clock::now();
    }
};

constexpr const char* kProbePage =
    "<!doctype html><html><head><title>kwe-web-probe</title></head><body>"
    "<canvas id=\"c\" width=\"160\" height=\"90\"></canvas><script>"
    "const c=document.getElementById('c');const x=c.getContext('2d');let i=0;"
    "function tick(){x.fillStyle='#101214';x.fillRect(0,0,160,90);"
    "x.fillStyle=i%2?'#1a4fae':'#ae3f1a';x.fillRect(i%157,i%77,3,3);i=(i+1)%256;"
    "requestAnimationFrame(tick);}requestAnimationFrame(tick);"
    "</script></body></html>";

// --probe: boot the real CEF browser against a throwaway animated page,
// require one CPU paint within a bounded deadline, and report a JSON
// backend record on stdout (consumed by `kwe diagnose`).
int run_probe(weweb::BrowserHost& host) {
    std::error_code ec;
    fs::path        probe_root =
        fs::temp_directory_path(ec) / ("kwe-web-probe-" + std::to_string(::getpid()));
    fs::create_directories(probe_root, ec);
    if (ec) {
        std::fprintf(stderr, "probe: cannot create %s\n", probe_root.c_str());
        return 1;
    }
    {
        FILE* page = std::fopen((probe_root / "index.html").c_str(), "w");
        if (! page) {
            std::fprintf(stderr, "probe: cannot write probe page\n");
            return 1;
        }
        std::fputs(kProbePage, page);
        std::fclose(page);
    }

    weweb::BrowserHost::InitOptions init;
    init.no_zygote = resolve_cef_dirs(init.resources_dir, init.locales_dir);
    init.cache_dir              = probe_root / "cache";
    init.enable_audio           = false;
    init.shared_texture_enabled = false;
    if (! host.Init(init)) {
        std::fprintf(stderr, "probe: BrowserHost::Init failed\n");
        return 1;
    }

    int  frames = 0;
    host.SetCpuPaintCallback([&frames](const weweb::CpuPaintFrame& frame) {
        if (frame.width > 0 && frame.height > 0) ++frames;
    });

    weweb::WebManifest manifest;
    manifest.title      = "kwe-web-probe";
    manifest.entry_html = "index.html";
    weweb::BrowserHost::OpenOptions open;
    open.shared_texture_enabled = false;
    open.frame_rate             = 30;
    if (! host.OpenWallpaper(manifest, probe_root, 160, 90, open)) {
        std::fprintf(stderr, "probe: OpenWallpaper failed\n");
        return 1;
    }

    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
    while (frames < 1 && std::chrono::steady_clock::now() < deadline && ! host.ShouldExit()) {
        host.Pump();
        host.Invalidate();
        ::usleep(10000);
    }
    host.Shutdown();
    fs::remove_all(probe_root, ec);

    if (frames < 1) {
        std::fprintf(stderr, "probe: no paint within the probe deadline\n");
        return 1;
    }
    std::printf("{\"backend\":\"cef-owe\",\"browser_version\":\"CEF/%s\","
                "\"frames\":%d,\"heartbeat\":\"paint-recency\"}\n",
                weweb::BrowserHost::CefVersionString().c_str(), frames);
    std::fflush(stdout);
    return 0;
}

} // namespace

int main(int argc, char** argv) {
    // CRITICAL: CEF re-execs this binary as its helper processes; the
    // helper check must run before any argv parsing or side effects.
    weweb::BrowserHost host;
    if (int helper_exit = host.RunOrExitIfHelper(argc, argv); helper_exit >= 0) {
        return helper_exit;
    }

    static rstd::log::EnvLogger logger;
    rstd::log::set_logger(logger);
    rstd::log::set_max_level(logger.filter());

    std::string parse_error;
    auto        args_opt = kwe::ParseWorkerArgs(argc, argv, /*web=*/true, parse_error);
    if (! args_opt) {
        std::fprintf(stderr, "kwe-web-renderer: %s\n", parse_error.c_str());
        return kwe::kExitUsage;
    }
    const kwe::WorkerArgs args = *args_opt;

    if (args.probe) {
        // The probe never needs the network; isolate unconditionally so the
        // report reflects the supervised (ungranted) configuration.
        std::string netns_error;
        if (! isolate_network(netns_error)) {
            std::fprintf(stderr, "probe: %s\n", netns_error.c_str());
        }
        return run_probe(host);
    }

    kwe::EmitStderrLines(args);
    kwe::InstallWorkerSignals(args.ignore_term);
    kwe::InputChannel input;

    if (args.startup_hang) {
        std::fprintf(stderr, "event=renderer.fault kind=startup_hang\n");
        kwe::ParkForever();
    }

    if (! args.allow_network) {
        std::string netns_error;
        if (! isolate_network(netns_error)) {
            // Fail closed: an ungranted wallpaper must never run with
            // network reachability.
            std::fprintf(stderr,
                         "event=renderer.error reason=backend_reject detail=netns_setup "
                         "error=%s\n",
                         netns_error.c_str());
            return kwe::kExitBackendReject;
        }
    }

    auto spec_opt = kwe::FrameSpec::Create(args.width, args.height);
    if (! spec_opt) {
        std::fprintf(stderr, "event=renderer.error reason=invalid_dimensions %ux%u\n",
                     args.width, args.height);
        return kwe::kExitUsage;
    }
    const kwe::FrameSpec spec = *spec_opt;

    std::string writer_error;
    auto writer_opt = kwe::SharedFrameWriter::Create(args.output, spec, writer_error);
    if (! writer_opt) {
        std::fprintf(stderr, "event=renderer.error reason=frame_mapping detail=%s\n",
                     writer_error.c_str());
        return 1;
    }
    kwe::SharedFrameWriter writer = std::move(*writer_opt);

    const fs::path content(args.content);
    auto           manifest = resolve_manifest(content);
    if (! manifest) {
        std::fprintf(stderr,
                     "event=renderer.error reason=no_drawable_content content=%s\n",
                     args.content.c_str());
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitNoDrawable;
    }

    weweb::BrowserHost::InitOptions init;
    init.no_zygote = resolve_cef_dirs(init.resources_dir, init.locales_dir);
    std::fprintf(stderr, "event=renderer.web.cef_resources dir=%s no_zygote=%d\n",
                 init.resources_dir.c_str(), int(init.no_zygote));
    init.cache_dir = cache_dir();
    // KWE policy: wallpaper audio output stays off (the daemon's audio
    // grant gates the analyzed-band delivery below, not output).
    init.enable_audio           = false;
    init.shared_texture_enabled = false;
    if (! host.Init(init)) {
        std::fprintf(stderr,
                     "event=renderer.error reason=backend_reject detail=browser_init_failed\n");
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitBackendReject;
    }

    PaintState paint;
    paint.spec = spec;
    paint.staging.assign(spec.pixel_bytes(), 0);
    host.SetCpuPaintCallback([&paint](const weweb::CpuPaintFrame& frame) {
        paint.CopyFrame(frame);
    });

    weweb::BrowserHost::OpenOptions open;
    open.shared_texture_enabled = false;
    open.frame_rate             = int(args.fps);
    if (! host.OpenWallpaper(*manifest, content, int(args.width), int(args.height), open)) {
        std::fprintf(stderr,
                     "event=renderer.error reason=backend_reject detail=open_wallpaper_failed\n");
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitBackendReject;
    }

    // Input wiring. Pointer coordinates arrive quantized to u16 across the
    // displayed image; CEF wants view pixels.
    bool                left_down = false;
    kwe::InputCallbacks callbacks;
    callbacks.on_pointer = [&host, &args, &left_down](const kwe::PointerEvent& event) {
        const int x = int(uint64_t(event.x) * (args.width - 1) / 65535);
        const int y = int(uint64_t(event.y) * (args.height - 1) / 65535);
        switch (event.phase) {
        case kwe::PointerPhase::Enter:
        case kwe::PointerPhase::Move: host.OnMouseMove(x, y, left_down); break;
        case kwe::PointerPhase::Leave: host.OnMouseMove(x, y, false); break;
        case kwe::PointerPhase::Down:
        case kwe::PointerPhase::Up: {
            // CEF cef_mouse_button_type_t: LEFT 0, MIDDLE 1, RIGHT 2.
            int button = 0;
            if (event.button == kwe::PointerButton::Middle) button = 1;
            if (event.button == kwe::PointerButton::Secondary) button = 2;
            const bool down = event.phase == kwe::PointerPhase::Down;
            if (button == 0) left_down = down;
            host.OnMouseButton(x, y, button, down, 1);
            break;
        }
        }
    };
    callbacks.on_audio_bands = [&host](const std::vector<float>& left,
                                       const std::vector<float>& right) {
        // Wallpaper Engine's web audio listener API expects 128 samples:
        // 64 per channel. Narrower daemon frames spread across the range.
        std::vector<float> response(128, 0.0f);
        const size_t       bands = left.size();
        for (size_t index = 0; index < 64; ++index) {
            const size_t source = index * bands / 64;
            response[index]      = left[source];
            response[index + 64] = right[source];
        }
        host.PushAudioData(response.data(), response.size());
    };

    const auto heartbeat_budget =
        std::chrono::milliseconds(args.web_heartbeat_ms * args.web_heartbeat_max_failures);
    uint64_t        published = 0;
    kwe::FramePacer pacer(args.fps);

    while (! kwe::TerminationFlag().load(std::memory_order_acquire)) {
        if (host.ShouldExit()) {
            std::fprintf(stderr,
                         "event=renderer.error reason=backend_reject detail=browser_closed\n");
            writer.SetState(kwe::ProducerState::Failed);
            return kwe::kExitBackendReject;
        }
        kwe::ApplyPublishFaults(args, published, writer);
        input.Poll(callbacks);

        host.Pump();
        // CEF's OSR pacing goes idle without explicit invalidate kicks;
        // the kick also makes paint recency a valid liveness signal.
        host.Invalidate();

        if (paint.have_frame) {
            published = writer.Publish(paint.staging.data());
            if (std::chrono::steady_clock::now() - paint.last_paint > heartbeat_budget) {
                std::fprintf(stderr,
                             "event=renderer.error reason=backend_reject detail=heartbeat "
                             "budget_ms=%llu\n",
                             (unsigned long long)heartbeat_budget.count());
                writer.SetState(kwe::ProducerState::Failed);
                return kwe::kExitBackendReject;
            }
        }
        pacer.WaitNext();
    }

    writer.SetState(kwe::ProducerState::Stopping);
    std::fprintf(stderr, "event=renderer.stop frames=%llu\n", (unsigned long long)published);
    host.RequestClose();
    host.Shutdown();
    return kwe::kExitGraceful;
}
