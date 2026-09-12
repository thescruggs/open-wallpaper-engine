# SPDX-License-Identifier: GPL-3.0-or-later
# F3: daemon-spawned occlusion detector bridge (session bus ↔ kwe-daemon).
qt_add_executable(kwe-occlusion-worker
    src/main.cpp
    src/occlusionbridge.cpp
    src/occlusionbridge.h
)

target_link_libraries(kwe-occlusion-worker PRIVATE
    Qt6::Core
    Qt6::DBus
    Qt6::Network
)

# The KWin script sits beside the binary in the build tree (dev runs resolve
# it there first) and under the data dir once installed.
configure_file(kwin/kwe-occlusion.js "${CMAKE_CURRENT_BINARY_DIR}/kwe-occlusion.js" COPYONLY)

install(TARGETS kwe-occlusion-worker RUNTIME DESTINATION bin)
install(
    FILES kwin/kwe-occlusion.js
    DESTINATION "${CMAKE_INSTALL_DATADIR}/kde-wallpaper-engine/kwin"
)

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()
