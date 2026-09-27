# SPDX-License-Identifier: GPL-3.0-or-later
# F4: daemon-spawned display power detector (compositor DPMS ↔ kwe-daemon).
# Reuses the occlusion worker's relay; only the source of the verdict and
# the daemon method differ.
qt_add_executable(kwe-display-power-worker
    src/main.cpp
    src/displaypowerwatcher.cpp
    src/displaypowerwatcher.h
    ../kwe-occlusion-worker/src/occlusionbridge.cpp
    ../kwe-occlusion-worker/src/occlusionbridge.h
)

target_link_libraries(kwe-display-power-worker PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Network
    KF6::ScreenDpms
)

install(TARGETS kwe-display-power-worker RUNTIME DESTINATION bin)
