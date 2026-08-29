// SPDX-License-Identifier: GPL-3.0-or-later
// kwe-scene-renderer (open-wallpaper-engine backend): the KDE Wallpaper
// Engine scene worker, rendered by owe::SceneWallpaper instead of the
// retired Rust renderer. The daemon spawns it as:
//
//   kwe-scene-renderer --output <frame> --width W --height H --fps N \
//       --scaling aspect|fill|stretch --content <scene.pkg|scene.json> \
//       [--assets-dir DIR] [--shader-helper PATH] [fault flags]
//
// stdin carries the NDJSON input pipe (pointer/audio/media), stdout the
// pointer acks, stderr the daemon's bounded diagnostic ring. Exit codes:
// 0 graceful SIGTERM, 70 exit-after fault, 71/72 memory-pressure fault,
// 73 backend rejection (scene unparseable, Vulkan unusable, engine
// bootstrap failure).
//
// Frames render fully offscreen: VulkanRender allocates its LINEAR-tiling
// LocalExSwapchain, this worker mmaps the exported DMA-BUF slots, converts
// RGBA -> premultiplied BGRA on the CPU, and publishes through the shared
// frame protocol (two-slot seqlock file). The --scaling mode maps onto the
// engine's FillMode so the GPU performs the aspect mapping.

#include <linux/dma-buf.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <map>
#include <memory>
#include <string>
#include <vector>

import rstd;
import rstd.cppstd;
import rstd.log;
import wescene.types;
import wescene.scene_wallpaper;
import wescene.pkg.parse;
import kwe.frame_protocol;
import kwe.input_wire;
import kwe.worker_common;

namespace
{

using namespace rstd::prelude;

// One mapped DMA-BUF slot of the local offscreen swapchain, cached across
// frames by slot id.
struct MappedSlot {
    void*  base { nullptr };
    size_t size { 0 };

    ~MappedSlot() {
        if (base) ::munmap(base, size);
    }
};

// Bounded wait on the frame's export sync fd. Some driver/export paths
// hand back an fd that never reports readiness through poll(2); after two
// consecutive timeouts the wait is skipped for the rest of the run (the
// DMA-BUF sync ioctl around the copy still orders the read) instead of
// stalling every publish tick.
void wait_render_complete(int sync_fd) {
    static int consecutive_timeouts = 0;
    if (sync_fd < 0) return;
    if (consecutive_timeouts >= 2) {
        ::close(sync_fd);
        return;
    }
    struct pollfd probe {};
    probe.fd      = sync_fd;
    probe.events  = POLLIN;
    const int ready = ::poll(&probe, 1, 100);
    if (ready <= 0) {
        if (++consecutive_timeouts == 2) {
            std::fprintf(stderr,
                         "event=renderer.sync_fd_unpollable detail=skipping_sync_waits\n");
        }
    } else {
        consecutive_timeouts = 0;
    }
    ::close(sync_fd);
}

void dmabuf_sync(int fd, uint64_t flags) {
    struct dma_buf_sync sync {};
    sync.flags = flags;
    // Best-effort: some exporters do not implement the sync ioctl; the
    // sync-file wait above already ordered the GPU write.
    (void)::ioctl(fd, DMA_BUF_IOCTL_SYNC, &sync);
}

owe::FillMode fill_mode_for(const std::string& scaling) {
    if (scaling == "stretch") return owe::FillMode::STRETCH;
    if (scaling == "fill") return owe::FillMode::ASPECTCROP;
    return owe::FillMode::ASPECTFIT; // "aspect": letterbox, the KWE default
}

std::string default_cache_dir() {
    const char* cache_home = std::getenv("XDG_CACHE_HOME");
    std::string base;
    if (cache_home && *cache_home) {
        base = cache_home;
    } else {
        const char* home = std::getenv("HOME");
        if (! home || ! *home) return {};
        base = std::string(home) + "/.cache";
    }
    return base + "/kwe-owe-scene";
}

} // namespace

int main(int argc, char** argv) {
    static rstd::log::EnvLogger logger;
    rstd::log::set_logger(logger);
    rstd::log::set_max_level(logger.filter());

    std::string parse_error;
    auto        args_opt = kwe::ParseWorkerArgs(argc, argv, /*web=*/false, parse_error);
    if (! args_opt) {
        std::fprintf(stderr, "kwe-scene-renderer: %s\n", parse_error.c_str());
        return kwe::kExitUsage;
    }
    const kwe::WorkerArgs args = *args_opt;

    kwe::EmitStderrLines(args);
    kwe::InstallWorkerSignals(args.ignore_term);
    kwe::InputChannel input; // configures nonblocking stdin/stdout early

    if (args.startup_hang) {
        std::fprintf(stderr, "event=renderer.fault kind=startup_hang\n");
        kwe::ParkForever();
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

    // Parse the scene before touching Vulkan so an unreadable or malformed
    // scene resolves as a pre-publish refusal (exit 73), never a crash loop.
    auto scene_document = owe::wpscene::LoadSceneDocumentFromSource(args.content);
    if (scene_document.is_none()) {
        std::fprintf(stderr,
                     "event=renderer.error reason=backend_reject detail=scene_unparseable "
                     "content=%s\n",
                     args.content.c_str());
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitBackendReject;
    }

    owe::SceneWallpaper wallpaper;
    if (! wallpaper.init()) {
        std::fprintf(stderr,
                     "event=renderer.error reason=backend_reject detail=engine_init_failed\n");
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitBackendReject;
    }
    wallpaper.setAudioClientIdentity({
        .application_name = "KDE Wallpaper Engine",
        .application_id   = "org.kde.kwe.scene-renderer",
        .stream_prefix    = "kwe-scene",
        .component        = "wescene",
        .media_name       = "KWE Scene Renderer",
        .media_role       = "music",
    });

    owe::SceneWallpaperConfig config;
    config.source_pkg_path = args.content;
    config.assets_dir      = args.assets_dir;
    config.cache_dir       = default_cache_dir();
    config.scene_document =
        std::make_shared<owe::wpscene::SceneDocument>(rstd::move(*scene_document));
    config.fps       = args.fps;
    config.fill_mode = fill_mode_for(args.scaling);
    // Global wallpaper-audio setting: --mute silences the engine's sound
    // manager (scene-authored sounds); frame output is unaffected.
    config.muted = args.mute;
    wallpaper.configure(rstd::move(config));

    {
        owe::RenderInitInfo info;
        info.offscreen              = true;
        info.offscreen_tiling       = owe::TexTiling::LINEAR;
        info.offscreen_host_visible = true;
        info.width            = uint16_t(args.width);
        info.height           = uint16_t(args.height);
        info.surface_info.createSurfaceOp = [](VkInstance, VkSurfaceKHR*) -> VkResult {
            return VK_SUCCESS;
        };
        wallpaper.initVulkan(rstd::move(info));
    }
    if (! wallpaper.waitVulkanInited(/*timeout_ms*/ 30000)) {
        std::fprintf(stderr,
                     "event=renderer.error reason=backend_reject detail=vulkan_init_timeout\n");
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitBackendReject;
    }
    owe::ExSwapchain* swapchain = wallpaper.exSwapchain();
    if (! swapchain) {
        std::fprintf(stderr,
                     "event=renderer.error reason=backend_reject detail=no_offscreen_swapchain\n");
        writer.SetState(kwe::ProducerState::Failed);
        return kwe::kExitBackendReject;
    }
    wallpaper.play();

    // Input wiring: quantized u16 coordinates are normalized straight into
    // the engine; media_state maps onto the scenescript MPRIS constants
    // (stopped 0, playing 1, paused 2). audio_bands cannot feed the engine
    // (it consumes PCM windows, not analyzed bands) and is ignored here;
    // the engine performs its own capture when the session permits it.
    kwe::InputCallbacks callbacks;
    callbacks.on_pointer = [&wallpaper](const kwe::PointerEvent& event) {
        const double x = double(event.x) / 65535.0;
        const double y = double(event.y) / 65535.0;
        switch (event.phase) {
        case kwe::PointerPhase::Enter:
            wallpaper.mouseEnter(true);
            wallpaper.mouseInput(x, y);
            break;
        case kwe::PointerPhase::Move:
            wallpaper.mouseEnter(true);
            wallpaper.mouseInput(x, y);
            break;
        case kwe::PointerPhase::Leave: wallpaper.mouseEnter(false); break;
        case kwe::PointerPhase::Down:
        case kwe::PointerPhase::Up: {
            int button = 0;
            if (event.button == kwe::PointerButton::Secondary) button = 1;
            if (event.button == kwe::PointerButton::Middle) button = 2;
            wallpaper.mouseInput(x, y);
            wallpaper.mouseButton(button, event.phase == kwe::PointerPhase::Down);
            break;
        }
        }
    };
    callbacks.on_media_state = [&wallpaper](const kwe::MediaStateEvent& event) {
        owe::MediaStatus status;
        status.state  = event.playback == "playing" ? 1u
                        : event.playback == "paused" ? 2u
                                                     : 0u;
        status.title  = event.title;
        status.artist = event.artist;
        status.album  = event.album;
        wallpaper.setMediaStatus(std::move(status));
    };

    std::map<int, MappedSlot> mapped_slots;
    std::vector<uint8_t>      staging(spec.pixel_bytes(), 0);
    bool                      have_frame = false;
    uint64_t                  published  = 0;
    kwe::FramePacer           pacer(args.fps);

    // Dev-only stage timing (KWE_SCENE_TIMING=1): microseconds spent per
    // stage, reported once a second on stderr.
    const bool timing_enabled = [] {
        const char* value = std::getenv("KWE_SCENE_TIMING");
        return value && *value && *value != '0';
    }();
    auto     now_us       = [] {
        struct timespec ts;
        clock_gettime(CLOCK_MONOTONIC, &ts);
        return uint64_t(ts.tv_sec) * 1000000u + uint64_t(ts.tv_nsec) / 1000u;
    };
    uint64_t stat_start   = now_us();
    uint64_t us_sync      = 0, us_copy = 0, us_publish = 0, us_pace = 0;
    uint64_t stat_frames  = 0, stat_loops = 0;

    while (! kwe::TerminationFlag().load(std::memory_order_acquire)) {
        kwe::ApplyPublishFaults(args, published, writer);
        input.Poll(callbacks);
        ++stat_loops;

        if (owe::ExHandle* handle = swapchain->eatFrame(); handle != nullptr) {
            ++stat_frames;
            const uint64_t t_sync = timing_enabled ? now_us() : 0;
            wait_render_complete(swapchain->takeLastFrameSyncFd());
            if (timing_enabled) us_sync += now_us() - t_sync;
            // Same-process fast path: read through the engine's cached
            // vkMapMemory pointer when the slot memory is host-visible.
            // The dmabuf mmap below stays as the fallback only — NVIDIA's
            // dmabuf mmap degrades to uncached reads (~12 MB/s) for row
            // pitches over 8 KiB, which collapsed every canvas wider than
            // 2048 px to 2-3 fps.
            const bool  fast_path = handle->host_ptr != nullptr;
            MappedSlot& slot      = mapped_slots[handle->id().to_primitive()];
            if (! fast_path && ! slot.base && handle->fd >= 0) {
                void* base =
                    ::mmap(nullptr, handle->size.to_primitive(), PROT_READ, MAP_SHARED,
                           handle->fd, 0);
                if (base == MAP_FAILED) {
                    std::fprintf(
                        stderr,
                        "event=renderer.error reason=backend_reject detail=dmabuf_mmap "
                        "errno=%d\n",
                        errno);
                    writer.SetState(kwe::ProducerState::Failed);
                    return kwe::kExitBackendReject;
                }
                slot.base = base;
                slot.size = handle->size.to_primitive();
            }
            if (fast_path || slot.base) {
                const uint64_t t_copy = timing_enabled ? now_us() : 0;
                if (! fast_path) dmabuf_sync(handle->fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
                const uint8_t* source =
                    (fast_path ? static_cast<const uint8_t*>(handle->host_ptr)
                               : static_cast<const uint8_t*>(slot.base)) +
                    size_t(handle->plane0_offset);
                const size_t source_stride = handle->plane0_stride
                                                 ? size_t(handle->plane0_stride)
                                                 : size_t(spec.stride);
                const uint32_t handle_height = uint32_t(handle->height.to_primitive());
                const uint32_t handle_width  = uint32_t(handle->width.to_primitive());
                const uint32_t rows    = handle_height < spec.height ? handle_height : spec.height;
                const uint32_t columns = handle_width < spec.width ? handle_width : spec.width;
                // The mapping is typically write-combined (uncached reads):
                // bulk-copy each row with memcpy's wide loads into cached
                // staging first, then swizzle in place at cache speed.
                for (uint32_t row = 0; row < rows; ++row) {
                    const uint8_t* in  = source + size_t(row) * source_stride;
                    uint8_t*       out = staging.data() + size_t(row) * spec.stride;
                    std::memcpy(out, in, size_t(columns) * 4);
                    for (uint32_t column = 0; column < columns; ++column) {
                        // VK_FORMAT_R8G8B8A8_UNORM -> BGRA8888.
                        const uint8_t red   = out[column * 4 + 0];
                        out[column * 4 + 0] = out[column * 4 + 2];
                        out[column * 4 + 2] = red;
                    }
                }
                if (! fast_path) dmabuf_sync(handle->fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
                if (timing_enabled) us_copy += now_us() - t_copy;
                have_frame = true;
            }
        }

        if (have_frame) {
            // Re-published even without a new engine frame so the
            // supervisor's generation watchdog sees liveness on static
            // scenes, exactly like the retired Rust worker.
            const uint64_t t_publish = timing_enabled ? now_us() : 0;
            published                = writer.Publish(staging.data());
            if (timing_enabled) us_publish += now_us() - t_publish;
        }
        {
            const uint64_t t_pace = timing_enabled ? now_us() : 0;
            pacer.WaitNext();
            if (timing_enabled) us_pace += now_us() - t_pace;
        }
        if (timing_enabled && now_us() - stat_start >= 1000000) {
            const uint64_t total = now_us() - stat_start;
            std::fprintf(stderr,
                         "timing loops=%llu engine_frames=%llu sync_us=%llu copy_us=%llu "
                         "publish_us=%llu pace_us=%llu other_us=%llu\n",
                         (unsigned long long)stat_loops,
                         (unsigned long long)stat_frames,
                         (unsigned long long)us_sync,
                         (unsigned long long)us_copy,
                         (unsigned long long)us_publish,
                         (unsigned long long)us_pace,
                         (unsigned long long)(total - us_sync - us_copy - us_publish - us_pace));
            stat_start = now_us();
            us_sync = us_copy = us_publish = us_pace = 0;
            stat_frames = stat_loops = 0;
        }
    }

    writer.SetState(kwe::ProducerState::Stopping);
    std::fprintf(stderr, "event=renderer.stop frames=%llu\n", (unsigned long long)published);
    // Skip SceneWallpaper teardown: the render thread owns Vulkan state and
    // the supervisor reaps the process; exiting is the reliable teardown.
    std::_Exit(kwe::kExitGraceful);
}
