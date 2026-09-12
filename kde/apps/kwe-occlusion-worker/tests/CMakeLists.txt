# SPDX-License-Identifier: GPL-3.0-or-later
qt_add_executable(kwe-occlusion-bridge-test
    occlusionbridgetest.cpp
    ../src/occlusionbridge.cpp
    ../src/occlusionbridge.h
)
target_link_libraries(kwe-occlusion-bridge-test PRIVATE Qt6::Core Qt6::DBus Qt6::Network Qt6::Test)
add_test(NAME kwe-occlusion-bridge-test COMMAND kwe-occlusion-bridge-test)
