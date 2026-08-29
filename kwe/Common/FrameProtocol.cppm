// SPDX-License-Identifier: GPL-3.0-or-later
// C++ producer for the KDE Wallpaper Engine shared frame protocol v1
// (kde/docs/FRAME_PROTOCOL_V1.md). Wire-compatible with the Rust
// `kwe-frame-protocol` crate: 64-byte little-endian header, two tightly
// packed BGRA8888 premultiplied slots, seqlock generation counter.
module;

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <atomic>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <functional>
#include <optional>
#include <string>

export module kwe.frame_protocol;

export namespace kwe
{

inline constexpr char     kFrameMagic[8]  = { 'K', 'W', 'E', 'F', 'R', 'M', '1', '\0' };
inline constexpr uint32_t kFrameVersion   = 1;
inline constexpr uint32_t kHeaderBytes    = 64;
inline constexpr uint32_t kSlotCount      = 2;
inline constexpr uint32_t kBytesPerPixel  = 4;
inline constexpr uint32_t kPixelFormatBgra8888Premultiplied = 1;
inline constexpr uint32_t kMaxDimension   = 8192;
inline constexpr uint64_t kMaxMappingBytes = 512ull * 1024 * 1024;

enum class ProducerState : uint32_t
{
    Starting = 1,
    Running  = 2,
    Stopping = 3,
    Failed   = 4,
};

struct FrameSpec {
    uint32_t width { 0 };
    uint32_t height { 0 };
    uint32_t stride { 0 };
    uint64_t slot_bytes { 0 };
    uint64_t file_bytes { 0 };

    static std::optional<FrameSpec> Create(uint32_t width, uint32_t height) {
        if (width == 0 || height == 0 || width > kMaxDimension || height > kMaxDimension) {
            return std::nullopt;
        }
        FrameSpec spec;
        spec.width      = width;
        spec.height     = height;
        spec.stride     = width * kBytesPerPixel;
        spec.slot_bytes = uint64_t(spec.stride) * uint64_t(height);
        spec.file_bytes = uint64_t(kHeaderBytes) + spec.slot_bytes * kSlotCount;
        if (spec.file_bytes > kMaxMappingBytes) return std::nullopt;
        return spec;
    }

    size_t pixel_bytes() const { return size_t(slot_bytes); }
};

// Producer-side mapping. Creation refuses to replace an existing path and
// opens with O_NOFOLLOW | O_CLOEXEC | 0600, matching the Rust writer.
class SharedFrameWriter {
public:
    SharedFrameWriter(const SharedFrameWriter&)            = delete;
    SharedFrameWriter& operator=(const SharedFrameWriter&) = delete;

    ~SharedFrameWriter() {
        if (m_mapping) ::munmap(m_mapping, size_t(m_spec.file_bytes));
        if (m_fd >= 0) ::close(m_fd);
    }

    static std::optional<SharedFrameWriter> Create(const std::string& path, FrameSpec spec,
                                                   std::string& error) {
        // Parent must be a real directory (no symlink component at the tail).
        const auto  slash  = path.find_last_of('/');
        std::string parent = slash == std::string::npos ? "." : path.substr(0, slash);
        if (parent.empty()) parent = "/";
        struct stat parent_stat {};
        if (::lstat(parent.c_str(), &parent_stat) != 0 || ! S_ISDIR(parent_stat.st_mode)) {
            error = "frame parent must be a real directory: " + parent;
            return std::nullopt;
        }
        int fd = ::open(
            path.c_str(), O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
        if (fd < 0) {
            error = "create frame mapping " + path + ": " + std::strerror(errno);
            return std::nullopt;
        }
        if (::ftruncate(fd, off_t(spec.file_bytes)) != 0) {
            error = "size frame mapping: " + std::string(std::strerror(errno));
            ::close(fd);
            return std::nullopt;
        }
        void* mapping = ::mmap(
            nullptr, size_t(spec.file_bytes), PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (mapping == MAP_FAILED) {
            error = "map frame file: " + std::string(std::strerror(errno));
            ::close(fd);
            return std::nullopt;
        }
        SharedFrameWriter writer;
        writer.m_fd      = fd;
        writer.m_mapping = static_cast<uint8_t*>(mapping);
        writer.m_spec    = spec;
        writer.initializeHeader();
        return writer;
    }

    SharedFrameWriter(SharedFrameWriter&& o) noexcept
        : m_fd(o.m_fd), m_mapping(o.m_mapping), m_spec(o.m_spec) {
        o.m_fd      = -1;
        o.m_mapping = nullptr;
    }
    SharedFrameWriter& operator=(SharedFrameWriter&& o) noexcept {
        if (this == &o) return *this;
        if (m_mapping) ::munmap(m_mapping, size_t(m_spec.file_bytes));
        if (m_fd >= 0) ::close(m_fd);
        m_fd        = o.m_fd;
        m_mapping   = o.m_mapping;
        m_spec      = o.m_spec;
        o.m_fd      = -1;
        o.m_mapping = nullptr;
        return *this;
    }

    const FrameSpec& spec() const { return m_spec; }

    // Seqlock publish: `fill` writes exactly `slot_bytes` of premultiplied
    // BGRA8888 into the inactive slot. Returns the published sequence.
    uint64_t PublishWith(const std::function<void(uint8_t* slot)>& fill) {
        const uint64_t starting = generation().load(std::memory_order_acquire);
        const uint64_t odd = (starting % 2 == 0) ? starting + 1 : starting + 2;
        generation().store(odd, std::memory_order_release);
        std::atomic_thread_fence(std::memory_order_seq_cst);

        const uint32_t current = active_slot().load(std::memory_order_relaxed);
        const uint32_t next    = (current + 1) % kSlotCount;
        fill(m_mapping + kHeaderBytes + size_t(m_spec.slot_bytes) * next);
        active_slot().store(next, std::memory_order_relaxed);
        state().store(uint32_t(ProducerState::Running), std::memory_order_relaxed);
        std::atomic_thread_fence(std::memory_order_release);
        const uint64_t even = odd + 1;
        generation().store(even, std::memory_order_release);
        return even / 2;
    }

    uint64_t Publish(const uint8_t* pixels) {
        return PublishWith([this, pixels](uint8_t* slot) {
            std::memcpy(slot, pixels, m_spec.pixel_bytes());
        });
    }

    void SetState(ProducerState value) {
        state().store(uint32_t(value), std::memory_order_release);
    }

    // Deliberately invalidates the magic for fault-injection tests.
    void CorruptMagicForTest() {
        std::memset(m_mapping, 0, sizeof(kFrameMagic));
        std::atomic_thread_fence(std::memory_order_seq_cst);
    }

private:
    SharedFrameWriter() = default;

    void initializeHeader() {
        std::memset(m_mapping, 0, size_t(m_spec.file_bytes));
        std::memcpy(m_mapping, kFrameMagic, sizeof(kFrameMagic));
        writeU32(8, kFrameVersion);
        writeU32(12, kHeaderBytes);
        writeU64(16, m_spec.file_bytes);
        writeU32(24, m_spec.width);
        writeU32(28, m_spec.height);
        writeU32(32, m_spec.stride);
        writeU32(36, kPixelFormatBgra8888Premultiplied);
        writeU32(40, kSlotCount);
        writeU64(48, 0);
        writeU32(56, 0);
        writeU32(60, uint32_t(ProducerState::Starting));
        ::msync(m_mapping, kHeaderBytes, MS_SYNC);
    }

    void writeU32(size_t offset, uint32_t value) {
        std::memcpy(m_mapping + offset, &value, sizeof(value));
    }
    void writeU64(size_t offset, uint64_t value) {
        std::memcpy(m_mapping + offset, &value, sizeof(value));
    }

    std::atomic_ref<uint64_t> generation() {
        return std::atomic_ref<uint64_t>(*reinterpret_cast<uint64_t*>(m_mapping + 48));
    }
    std::atomic_ref<uint32_t> active_slot() {
        return std::atomic_ref<uint32_t>(*reinterpret_cast<uint32_t*>(m_mapping + 56));
    }
    std::atomic_ref<uint32_t> state() {
        return std::atomic_ref<uint32_t>(*reinterpret_cast<uint32_t*>(m_mapping + 60));
    }

    int       m_fd { -1 };
    uint8_t*  m_mapping { nullptr };
    FrameSpec m_spec;
};

} // namespace kwe
