# Builds the smoke programs against an installed libqucelerate-ai-gateway, found through
# its CMake package (CMAKE_PREFIX_PATH names the install prefix).
cmake_minimum_required(VERSION 3.16)
project(gateway_smoke LANGUAGES C CXX)
find_package(qucelerate-ai-gateway REQUIRED)
add_executable(smoke_c smoke.c)
target_link_libraries(smoke_c PRIVATE qucelerate::ai-gateway)
add_executable(smoke_cpp smoke.cpp)
target_compile_features(smoke_cpp PRIVATE cxx_std_17)
target_link_libraries(smoke_cpp PRIVATE qucelerate::ai-gateway)
